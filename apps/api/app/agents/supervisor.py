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


class DeterministicSupervisor:
    """Model-free fallback: fixed plan over the same tools. Labeled mode='deterministic'."""

    def __init__(self, ctx: ToolContext):
        self.ctx = ctx
        self.tools = {t.name: t for t in build_tools(ctx)}

    def chat(self, message: str) -> ChatResponse:
        msg = message.lower()
        if any(w in msg for w in ("find", "search", "analy", "compare", "rank", "properties")):
            ranking = json.loads(self.tools["get_ranking"].invoke({}))
            if ranking.get("status") == "NO_RANKING":
                self.tools["run_property_analysis"].invoke({})
                ranking = json.loads(self.tools["get_ranking"].invoke({}))
            lines = [
                f"#{r['rank']}: " + ranking["explanations"][str(r["property_id"])] for r in ranking["ranked"]
            ]
            reply = "Ranking (deterministic engine):\n" + "\n".join(lines)
        else:
            state = json.loads(self.tools["get_project_state"].invoke({}))
            reply = (
                f"Project '{state['project']['project_name']}' is in status {state['project']['project_status']}. "
                "Ask me to find and compare properties."
            )
        return ChatResponse(reply=reply, mode="deterministic", grounded_record_ids=self.ctx.touched_records)


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
