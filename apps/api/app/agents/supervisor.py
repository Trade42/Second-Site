"""Expansion Supervisor built on LangChain Deep Agents (`deepagents.create_deep_agent`, verified v0.7.x).

- Specialists are Deep Agents subagents (`SubAgent` dicts: name, description, system_prompt, tools).
- Arithmetic, matching, ranking and persistence happen in deterministic tools, not in the model.
- External actions only via `request_outreach_approval` (approval_guard) — the supervisor cannot send.
- Checkpointing: LangGraph checkpointer (InMemorySaver by default; PostgresSaver when
  AGENT_CHECKPOINTER=postgres). Checkpoints are agent execution state, NOT business state.
- No model configured -> `DeterministicSupervisor` runs the same tools in a fixed plan (labeled mode).
"""

from __future__ import annotations

import json
from typing import Any
from uuid import UUID

from langchain_core.language_models import BaseChatModel
from langchain_core.messages import AIMessage

from app.contracts import ChatResponse
from app.core.config import Settings

from .backend_client import MAX_MESSAGE_CHARS, TOOL_ERROR_STATUS
from .tools import ToolContext, build_tools

SUPERVISOR_PROMPT = """You are the Second Site Expansion Supervisor. You help a business owner open their next
location. Ground every statement in tool results and cite record IDs. Never invent listings, prices,
permits or infrastructure facts. Historical permits do not prove current capacity; say UNKNOWN when unknown.
You cannot contact anyone yourself: use request_outreach_approval, which only asks the owner.
Delegate evidence questions to the feasibility subagent and compliance checks to the evidence subagent."""

SUBAGENTS: list[dict[str, Any]] = [
    {
        "name": "requirements",
        "description": "Clarifies and summarizes the business expansion profile.",
        "system_prompt": "Summarize the confirmed business profile. Do not change it.",
        "tool_names": ["get_project_state"],
    },
    {
        "name": "feasibility",
        "description": "Explains infrastructure assessments and evidence for a property.",
        "system_prompt": "Explain assessments strictly from tool output; preserve UNKNOWN and limitations.",
        "tool_names": ["get_feasibility", "get_property_evidence"],
    },
    {
        "name": "financial",
        "description": "Explains cost scenarios and ranking tradeoffs.",
        "system_prompt": "Explain costs from tool output only. Amounts are integer cents.",
        "tool_names": ["get_cost_scenarios", "get_ranking"],
    },
    {
        "name": "evidence",
        "description": "Validates that claims are grounded in sourced evidence.",
        "system_prompt": "Run validate_evidence and report problems verbatim.",
        "tool_names": ["validate_evidence"],
    },
]


def build_checkpointer(settings: Settings):
    if settings.agent_checkpointer == "postgres":
        from langgraph.checkpoint.postgres import (
            PostgresSaver,  # requires setup() once (scripts/bootstrap.sh)
        )

        url = settings.database_url.replace("postgresql+psycopg://", "postgresql://")
        return PostgresSaver.from_conn_string(url)
    from langgraph.checkpoint.memory import InMemorySaver

    return InMemorySaver()


_MEMORY_CHECKPOINTER = None


def create_supervisor(ctx: ToolContext, model: str | BaseChatModel, checkpointer=None):
    from deepagents import create_deep_agent

    tools = build_tools(ctx)
    by_name = {t.name: t for t in tools}
    subagents = [
        {
            "name": s["name"],
            "description": s["description"],
            "system_prompt": s["system_prompt"],
            "tools": [by_name[n] for n in s["tool_names"]],
        }
        for s in SUBAGENTS
    ]
    return create_deep_agent(
        model=model,
        tools=tools,
        system_prompt=SUPERVISOR_PROMPT,
        subagents=subagents,
        checkpointer=checkpointer,
        name="expansion-supervisor",
    )


class UnusableToolAnswer(Exception):
    """A tool answer the fixed plan cannot use. kind 'error': the tool's own error text (HTTP mode: a Rust error,
    timeout or outage, encoded by `BackendAPIError.tool_text`). kind 'malformed': not JSON, or lacks the expected keys.
    """

    def __init__(self, tool: str, kind: str, error_code: str, message: str):
        super().__init__(f"{tool}: {error_code}: {message}")
        self.tool = tool
        self.kind = kind
        self.error_code = error_code
        self.message = message[:MAX_MESSAGE_CHARS]


def _malformed(tool: str, message: str, error_code: str = "BAD_TOOL_ANSWER") -> UnusableToolAnswer:
    return UnusableToolAnswer(tool, "malformed", error_code, message)


def _failure_reply(what: str, failure: UnusableToolAnswer, analysis_ran: bool) -> str:
    """Says plainly that the backend failed; never worded as a ranking or a project status."""
    if failure.kind == "error":
        head = f"No {what} to show: the backend could not be reached or returned an error."
    else:
        head = f"No {what} to show: the backend returned an answer without the expected content."
    reply = f"{head} {failure.tool}: {failure.error_code}: {failure.message}".rstrip()
    if not reply.endswith((".", "!", "?")):
        reply += "."
    if analysis_ran:
        reply += " The property analysis ran before this failure."
    return reply


class DeterministicSupervisor:
    """Model-free fallback: fixed plan over the same tools. Labeled mode='deterministic'.

    A tool error text (HTTP mode) or an unusable answer becomes an honest reply, never a ranking or a status.
    Exceptions a tool raises still propagate, as before (in-process tools raise; they never return error texts)."""

    def __init__(self, ctx: ToolContext):
        self.ctx = ctx
        self.tools = {t.name: t for t in build_tools(ctx)}

    def _answer(self, tool: str) -> dict[str, Any]:
        """The tool's JSON object, or UnusableToolAnswer for an error text, non-JSON text or a non-object."""
        text = self.tools[tool].invoke({})
        try:
            out = json.loads(text)
        except (TypeError, ValueError):
            raise _malformed(tool, "the answer is not JSON") from None
        if not isinstance(out, dict):
            raise _malformed(tool, "the answer is not a JSON object")
        if out.get("status") == TOOL_ERROR_STATUS:  # the shape of BackendAPIError.tool_text()
            code, message = out.get("error_code"), out.get("message")
            raise UnusableToolAnswer(
                tool,
                "error",
                code if isinstance(code, str) and code else TOOL_ERROR_STATUS,
                message if isinstance(message, str) else "",
            )
        return out

    def chat(self, message: str) -> ChatResponse:
        msg = message.lower()
        analysis_ran = False
        if any(w in msg for w in ("find", "search", "analy", "compare", "rank", "properties")):
            try:
                ranking = self._answer("get_ranking")
                if ranking.get("status") == "NO_RANKING":
                    self._answer("run_property_analysis")
                    analysis_ran = True
                    ranking = self._answer("get_ranking")
                reply = "Ranking (deterministic engine):\n" + "\n".join(_ranking_lines(ranking))
            except UnusableToolAnswer as failure:
                reply = _failure_reply("ranking", failure, analysis_ran)
        else:
            try:
                project = _project(self._answer("get_project_state"))
                reply = (
                    f"Project '{project['project_name']}' is in status {project['project_status']}. "
                    "Ask me to find and compare properties."
                )
            except UnusableToolAnswer as failure:
                reply = _failure_reply("project status", failure, analysis_ran)
        return ChatResponse(reply=reply, mode="deterministic", grounded_record_ids=self.ctx.touched_records)


def _ranking_lines(ranking: dict[str, Any]) -> list[str]:
    """'#rank: explanation' per ranked property, in order; UnusableToolAnswer if the ranking lacks its keys."""
    if ranking.get("status") == "NO_RANKING":
        raise _malformed("get_ranking", "no ranking exists after the property analysis", "NO_RANKING")
    ranked, explanations = ranking.get("ranked"), ranking.get("explanations")
    if not isinstance(ranked, list) or not isinstance(explanations, dict):
        raise _malformed("get_ranking", "the answer has no ranked properties with explanations")
    lines = []
    for r in ranked:
        explanation = (
            explanations.get(str(r["property_id"])) if isinstance(r, dict) and "property_id" in r else None
        )
        if not isinstance(explanation, str) or "rank" not in r:
            raise _malformed("get_ranking", "a ranked property has no rank or explanation")
        lines.append(f"#{r['rank']}: " + explanation)
    return lines


def _project(state: dict[str, Any]) -> dict[str, Any]:
    project = state.get("project")
    if not isinstance(project, dict) or not {"project_name", "project_status"} <= project.keys():
        raise _malformed(
            "get_project_state", "the answer has no project with project_name and project_status"
        )
    return project


def run_chat(
    ctx: ToolContext,
    message: str,
    settings: Settings,
    model: str | BaseChatModel | None = None,
    thread_id: str | None = None,
) -> ChatResponse:
    global _MEMORY_CHECKPOINTER
    model = model or settings.supervisor_model
    if not model:
        return DeterministicSupervisor(ctx).chat(message)
    if settings.agent_checkpointer == "memory":
        if _MEMORY_CHECKPOINTER is None:
            _MEMORY_CHECKPOINTER = build_checkpointer(settings)
        cp = _MEMORY_CHECKPOINTER
    else:
        cp = None  # postgres saver context managed by worker deployments; see docs/AGENT_ARCHITECTURE.md
    agent = create_supervisor(ctx, model, cp)
    cfg = {"configurable": {"thread_id": thread_id or f"project:{ctx.project_id}"}, "recursion_limit": 40}
    result = agent.invoke({"messages": [{"role": "user", "content": message}]}, config=cfg)
    last = next((m for m in reversed(result["messages"]) if isinstance(m, AIMessage)), None)
    text = (
        last.content if last and isinstance(last.content, str) else json.dumps(getattr(last, "content", ""))
    )
    return ChatResponse(
        reply=text, mode="deep_agent", grounded_record_ids=list(dict.fromkeys(ctx.touched_records))
    )


def project_thread(project_id: UUID) -> str:
    return f"project:{project_id}"
