"""The Deep Agents supervisor really executes structured tools (scripted chat model, no network)."""

from __future__ import annotations

from uuid import uuid4

from langchain_core.language_models.fake_chat_models import GenericFakeChatModel
from langchain_core.messages import AIMessage

from app.agents.supervisor import create_supervisor
from app.agents.tools import ToolContext, build_tools
from app.contracts import ApprovalStatus, ProjectContext
from app.db import models as m
from app.repositories import registry as repos
from app.services.authorization import ToolCategory

from .helpers import demo_profile


class ScriptedToolModel(GenericFakeChatModel):
    def bind_tools(self, tools, **kwargs):  # deepagents binds tools; the script already knows them
        return self


def _ctx(db, container):
    owner = uuid4()
    db.add(m.User(id=owner, display_name="o"))
    db.flush()
    p = ProjectContext(owner_id=owner, project_name="t", market={"city": "Los Angeles", "state": "CA"})
    repos.projects.add(db, p)
    repos.profiles.add(db, demo_profile(opening_capital=None).model_copy(update={"project_id": p.project_id}))
    return ToolContext(db=db, c=container, project_id=p.project_id, owner_id=owner)


def test_deep_agent_supervisor_delegates_to_structured_tools(db, container):
    from langgraph.checkpoint.memory import InMemorySaver

    ctx = _ctx(db, container)
    script = iter(
        [
            AIMessage(content="", tool_calls=[{"name": "run_property_analysis", "args": {}, "id": "c1"}]),
            AIMessage(content="", tool_calls=[{"name": "get_ranking", "args": {}, "id": "c2"}]),
            AIMessage(
                content="",
                tool_calls=[
                    {
                        "name": "request_outreach_approval",
                        "id": "c3",
                        "args": {
                            "property_id": "p",
                            "recipient_name": "Broker",
                            "recipient_phone": "+15555550100",
                            "questions": "panel size?",
                        },
                    }
                ],
            ),
            AIMessage(content="Hollywood ranks first; outreach awaits your approval."),
        ]
    )
    agent = create_supervisor(ctx, ScriptedToolModel(messages=script), InMemorySaver())
    res = agent.invoke(
        {"messages": [{"role": "user", "content": "Compare properties"}]},
        config={"configurable": {"thread_id": "t1"}},
    )
    tool_msgs = [mm for mm in res["messages"] if mm.type == "tool"]
    assert [t.name for t in tool_msgs] == [
        "run_property_analysis",
        "get_ranking",
        "request_outreach_approval",
    ]
    assert len(repos.rankings.list(db, project_id=ctx.project_id)) == 1  # persisted by the deterministic tool
    appr = repos.approvals.list(db, project_id=ctx.project_id)
    assert len(appr) == 1 and appr[0].approval_status == ApprovalStatus.PENDING  # nothing was sent
    assert "APPROVAL_REQUIRED" in tool_msgs[-1].content


def test_tool_categories_gate_external_actions(db, container):
    from app.agents.tools import TOOL_CATEGORIES

    build_tools(_ctx(db, container))
    assert TOOL_CATEGORIES["request_outreach_approval"] == ToolCategory.EXTERNAL_COMMUNICATION
    assert TOOL_CATEGORIES["get_ranking"] == ToolCategory.READ_ONLY
    assert not any(
        cat == ToolCategory.CONSEQUENTIAL for cat in TOOL_CATEGORIES.values()
    )  # no purchase tool exposed


def test_harness_keeps_the_tools_in_process_whatever_the_environment_says(db, container, tmp_path):
    # conftest forces BACKEND_API_URL to "" (R7): neither an exported URL nor a .env entry may switch this suite to
    # HTTP mode, and an empty value must mean "unset" (in-process tools, no HTTP client).
    import os

    from app.agents.backend_client import backend_client
    from app.core.config import Settings

    assert os.environ.get("BACKEND_API_URL") == ""
    dotenv = tmp_path / ".env"
    dotenv.write_text("BACKEND_API_URL=http://from-dotenv.invalid\n")
    assert Settings(_env_file=dotenv).backend_api_url == ""  # the environment wins over .env
    assert container.settings.backend_api_url == ""
    assert backend_client(container.settings, uuid4()) is None
    assert not any(t.handle_tool_error for t in build_tools(_ctx(db, container)))
