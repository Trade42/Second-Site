"""Chat on the owner's ChatGPT subscription through the Codex CLI (`SUPERVISOR_MODEL=codex:...`).

Every test puts a fake `codex` first on PATH: a small script that records its argv, standard input, working
directory and environment, then answers, fails, prints nothing or sleeps. No test calls the real CLI or the network.
"""

from __future__ import annotations

import json
import logging
import os
import stat
import sys
from pathlib import Path
from uuid import UUID, uuid4

import pytest

from app.agents import codex_chat as cc
from app.agents.supervisor import run_chat
from app.agents.tools import ToolContext, build_tools
from app.contracts import ProjectContext
from app.db import models as m
from app.repositories import registry as repos

from .helpers import demo_profile

MODEL = "codex:gpt-6.1-terra:low"
CANNED = "Property ranks first on cost (fixture demo data)."


def _fake_cli(
    tmp_path: Path, *, answer: str = CANNED, exit_code: int = 0, stderr: str = "", sleep: float = 0
):
    """Write a fake `codex` into its own directory; returns (bin_dir, record_file)."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir(exist_ok=True)
    record = tmp_path / "record.json"
    script = bin_dir / "codex"
    script.write_text(
        f"""#!{sys.executable}
import json, os, sys, time
argv = sys.argv[1:]
stdin = sys.stdin.read()
cwd = os.getcwd()
json.dump({{"argv": argv, "stdin": stdin, "cwd": cwd, "cwd_files": os.listdir(cwd), "env": dict(os.environ)}},
          open({str(record)!r}, "w"))
time.sleep({sleep!r})
sys.stderr.write({stderr!r})
if {exit_code!r} == 0:
    out = argv[argv.index("-o") + 1]
    open(out, "w").write({answer!r})
sys.exit({exit_code!r})
"""
    )
    script.chmod(script.stat().st_mode | stat.S_IXUSR)
    return bin_dir, record


@pytest.fixture(autouse=True)
def _fresh_history():
    cc.clear_history()
    yield
    cc.clear_history()


@pytest.fixture
def ranked_ctx(db, container):
    owner = uuid4()
    db.add(m.User(id=owner, display_name="o"))
    db.flush()
    p = ProjectContext(owner_id=owner, project_name="t", market={"city": "Los Angeles", "state": "CA"})
    repos.projects.add(db, p)
    repos.profiles.add(db, demo_profile().model_copy(update={"project_id": p.project_id}))
    ctx = ToolContext(db=db, c=container, project_id=p.project_id, owner_id=owner)
    {t.name: t for t in build_tools(ctx)}["run_property_analysis"].invoke({})
    ctx.touched_records.clear()
    return ctx


def _use(monkeypatch, bin_dir: Path):
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")


# ---- the setting ---------------------------------------------------------------------------------------------
def test_parse_spec():
    assert cc.parse_spec("codex:gpt-6.1-terra:low") == cc.CodexSpec("gpt-6.1-terra", "low")
    assert cc.parse_spec(" codex:gpt-5.6-terra ") == cc.CodexSpec("gpt-5.6-terra", None)
    assert cc.is_codex_model("codex:x") and not cc.is_codex_model("anthropic:claude-sonnet-5-5")
    for bad in ("codex:", "codex:gpt;rm -rf:low", "codex:gpt-6.1-terra:ultra", "codex:-m:low"):
        with pytest.raises(ValueError):
            cc.parse_spec(bad)


def test_command_line_is_exact():
    argv = cc.command("/usr/bin/codex", cc.CodexSpec("gpt-6.1-terra", "low"), "/tmp/w", "/tmp/o.txt")
    assert argv == [
        "/usr/bin/codex", "exec", "-m", "gpt-6.1-terra", "-c", 'model_reasoning_effort="low"',
        "-s", "read-only", "-C", "/tmp/w", "--skip-git-repo-check", "--ephemeral",
        "--ignore-user-config", "--ignore-rules", "--color", "never",
        "--disable", "shell_tool", "--disable", "unified_exec", "--disable", "browser_use",
        "--disable", "computer_use", "--disable", "in_app_browser",
        "-o", "/tmp/o.txt", "-",
    ]  # fmt: skip
    no_effort = cc.command("codex", cc.CodexSpec("m", None), "/w", "/o")
    assert "-c" not in no_effort


# ---- a reply from the model -------------------------------------------------------------------------------
def test_one_cli_call_with_facts_and_no_secret(ranked_ctx, container, tmp_path, monkeypatch, caplog):
    bin_dir, record = _fake_cli(tmp_path)
    _use(monkeypatch, bin_dir)
    monkeypatch.setenv("SECRET_PROBE", "probe-value-never-sent")
    caplog.set_level(logging.DEBUG)

    res = run_chat(ranked_ctx, "Which space ranks first and why?", container.settings, model=MODEL)

    assert res.reply == CANNED and res.mode == "codex_cli:gpt-6.1-terra:low"
    rec = json.loads(record.read_text())
    argv = rec["argv"]
    assert argv[:4] == ["exec", "-m", "gpt-6.1-terra", "-c"] and argv[4] == 'model_reasoning_effort="low"'
    assert argv[argv.index("-s") + 1] == "read-only"
    assert {"--skip-git-repo-check", "--ephemeral", "--ignore-user-config"} <= set(argv)
    assert argv[argv.index("-C") + 1] == rec["cwd"] and rec["cwd_files"] == []  # empty working directory
    assert not Path(rec["cwd"]).exists()  # removed after the call
    assert argv[-1] == "-" and "Which space ranks first" not in " ".join(argv)  # prompt only on stdin
    # Only the pass-through variables reach the CLI: no app setting, key or database URL.
    assert set(rec["env"]) <= set(cc.ENV_PASSTHROUGH)
    # The prompt holds the rules, the facts and the message, and nothing secret.
    stdin = rec["stdin"]
    ranking = repos.rankings.list(ranked_ctx.db, project_id=ranked_ctx.project_id)[-1]
    first = ranking.ranked_properties[0].property_id
    assert "Answer only from the FACTS" in stdin and "fixture (demo) data" in stdin
    assert str(ranking.ranking_id) in stdin and str(first) in stdin
    assert stdin.rstrip().endswith("Write the assistant's reply.")
    settings = container.settings
    for secret in (settings.auth_secret.get_secret_value(), settings.database_url, "probe-value-never-sent"):
        assert secret not in stdin and secret not in json.dumps(rec["env"])
    assert ranking.ranking_id in res.grounded_record_ids and first in res.grounded_record_ids
    assert ranked_ctx.project_id in res.grounded_record_ids
    assert "probe-value" not in caplog.text and settings.auth_secret.get_secret_value() not in caplog.text


def test_recent_turns_go_with_the_next_message(ranked_ctx, container, tmp_path, monkeypatch):
    bin_dir, record = _fake_cli(tmp_path)
    _use(monkeypatch, bin_dir)
    run_chat(ranked_ctx, "Which space ranks first?", container.settings, model=MODEL, thread_id="t1")
    run_chat(ranked_ctx, "And its electrical service?", container.settings, model=MODEL, thread_id="t1")
    stdin = json.loads(record.read_text())["stdin"]
    assert "Owner: Which space ranks first?" in stdin and f"Assistant: {CANNED}" in stdin
    run_chat(ranked_ctx, "Hello", container.settings, model=MODEL, thread_id="other")
    assert "RECENT CONVERSATION" not in json.loads(record.read_text())["stdin"]


def test_no_ranking_is_stated_and_the_analysis_is_not_run(db, container, tmp_path, monkeypatch):
    owner = uuid4()
    db.add(m.User(id=owner, display_name="o"))
    db.flush()
    p = ProjectContext(owner_id=owner, project_name="t", market={"city": "Los Angeles", "state": "CA"})
    repos.projects.add(db, p)
    ctx = ToolContext(db=db, c=container, project_id=p.project_id, owner_id=owner)
    bin_dir, record = _fake_cli(tmp_path)
    _use(monkeypatch, bin_dir)
    res = run_chat(ctx, "Which space ranks first?", container.settings, model=MODEL)
    assert res.mode.startswith("codex_cli:")
    assert "NO_RANKING" in json.loads(record.read_text())["stdin"]
    assert repos.rankings.list(db, project_id=p.project_id) == []


# ---- an action request ---------------------------------------------------------------------------------------
@pytest.mark.parametrize(
    "message",
    [
        "Call the broker for the first space",
        "Please email the landlord",
        "Buy the chairs",
        "approve the outreach",
    ],
)
def test_action_request_is_not_sent_to_the_model_or_reported_done(
    ranked_ctx, container, tmp_path, monkeypatch, message
):
    bin_dir, record = _fake_cli(tmp_path, answer="I called the broker and it is done.")
    _use(monkeypatch, bin_dir)
    res = run_chat(ranked_ctx, message, container.settings, model=MODEL)
    assert not record.exists()  # the CLI never ran
    assert res.mode == "deterministic:action_request"
    assert "Nothing was sent, called, bought or approved" in res.reply and "done" not in res.reply
    assert repos.approvals.list(ranked_ctx.db, project_id=ranked_ctx.project_id) == []


def test_questions_are_not_mistaken_for_actions():
    for q in ("Which space ranks first and why?", "Is the electrical service enough at the first one?"):
        assert not cc.is_action_request(q)


# ---- failure is said ---------------------------------------------------------------------------------------
@pytest.mark.parametrize(
    ("kwargs", "why"),
    [
        ({"exit_code": 1, "stderr": "Error: Not logged in. Run codex login. token=sk-leak"}, "not signed in"),
        ({"exit_code": 3, "stderr": "boom sk-leak"}, "exit code 3"),
        ({"answer": "   "}, "empty answer"),
    ],
)
def test_cli_failure_is_said_without_its_output(
    ranked_ctx, container, tmp_path, monkeypatch, caplog, kwargs, why
):
    bin_dir, _ = _fake_cli(tmp_path, **kwargs)
    _use(monkeypatch, bin_dir)
    caplog.set_level(logging.DEBUG)
    res = run_chat(ranked_ctx, "Which space ranks first?", container.settings, model=MODEL)
    assert res.reply.startswith("The assistant's model could not be reached") and why in res.reply
    assert res.mode == "codex_cli_unavailable:gpt-6.1-terra:low" and res.grounded_record_ids == []
    assert "sk-leak" not in res.reply and "sk-leak" not in caplog.text


def test_timeout_kills_the_cli_and_says_so(ranked_ctx, container, tmp_path, monkeypatch):
    bin_dir, record = _fake_cli(tmp_path, sleep=30)
    _use(monkeypatch, bin_dir)
    monkeypatch.setattr(cc, "TIMEOUT_S", 1)
    res = run_chat(ranked_ctx, "Which space ranks first?", container.settings, model=MODEL)
    assert record.exists()
    assert "did not answer within 1 seconds" in res.reply and res.mode.startswith("codex_cli_unavailable:")


def test_missing_cli_is_said(ranked_ctx, container, tmp_path, monkeypatch):
    empty = tmp_path / "empty"
    empty.mkdir()
    monkeypatch.setenv("PATH", str(empty))
    res = run_chat(ranked_ctx, "Which space ranks first?", container.settings, model=MODEL)
    assert "Codex CLI is not installed" in res.reply and res.mode.startswith("codex_cli_unavailable:")


def test_invalid_setting_is_said(ranked_ctx, container, tmp_path, monkeypatch):
    bin_dir, record = _fake_cli(tmp_path)
    _use(monkeypatch, bin_dir)
    res = run_chat(ranked_ctx, "hi", container.settings, model="codex:gpt-6.1-terra:turbo")
    assert not record.exists() and res.mode == "codex_cli_unavailable:invalid_setting"
    assert "invalid codex: setting" in res.reply


def test_chat_route_answers_200_on_a_model_failure(client, auth, demo_project_body, tmp_path, monkeypatch):
    bin_dir, _ = _fake_cli(tmp_path, exit_code=1, stderr="401 Unauthorized")
    _use(monkeypatch, bin_dir)
    from app.core.container import get_container

    monkeypatch.setattr(get_container().settings, "supervisor_model", MODEL)
    h, _ = auth
    pid = client.post("/api/v1/projects", json=demo_project_body, headers=h).json()["project"]["project_id"]
    r = client.post(f"/api/v1/projects/{pid}/chat", json={"message": "Which space ranks first?"}, headers=h)
    assert r.status_code == 200
    body = r.json()
    assert body["mode"] == "codex_cli_unavailable:gpt-6.1-terra:low" and "not signed in" in body["reply"]
    UUID(pid)


# ---- review fixes (assistant-codex-1-review) ----------------------------------------------------------------
def _raw_fake(tmp_path: Path, body: str) -> Path:
    bin_dir = tmp_path / "rawbin"
    bin_dir.mkdir()
    script = bin_dir / "codex"
    script.write_text(
        f"#!{sys.executable}\nimport sys\nargv = sys.argv[1:]\nout = argv[argv.index('-o') + 1]\n{body}\n"
    )
    script.chmod(script.stat().st_mode | stat.S_IXUSR)
    return bin_dir


@pytest.mark.parametrize(
    "body",
    [
        "sys.stdin.read(); sys.stderr.buffer.write(b'\\xff\\xfe bad'); sys.exit(1)",
        "sys.stdin.read(); sys.stdout.buffer.write(b'\\xff bad'); open(out, 'wb').write(b'ok'); sys.exit(0)",
    ],
)
def test_invalid_utf8_from_the_cli_does_not_raise(tmp_path, monkeypatch, body):
    _use(monkeypatch, _raw_fake(tmp_path, body))
    spec = cc.parse_spec(MODEL)
    try:
        cc.run_codex(spec, "q")
    except cc.CodexUnavailable as exc:
        assert exc.kind == "exit"


def test_cli_that_cannot_start_is_said(tmp_path, monkeypatch):
    bin_dir = tmp_path / "nox"
    bin_dir.mkdir()
    (bin_dir / "codex").write_text("#!/nonexistent/interpreter\n")
    (bin_dir / "codex").chmod(0o755)
    _use(monkeypatch, bin_dir)
    with pytest.raises(cc.CodexUnavailable) as exc:
        cc.run_codex(cc.parse_spec(MODEL), "q")
    assert exc.value.kind == "start"


def test_long_reply_is_cut_and_action_claim_gets_the_notice(tmp_path, monkeypatch):
    _use(monkeypatch, _raw_fake(tmp_path, "sys.stdin.read(); open(out, 'w').write('A' * 200000)"))
    long = cc.run_codex(cc.parse_spec(MODEL), "q")
    assert len(long) <= cc.MAX_REPLY_CHARS + 40 and long.endswith("[reply cut: too long]")
    claim = tmp_path / "claim"
    claim.mkdir()
    _use(
        monkeypatch, _raw_fake(claim, "sys.stdin.read(); open(out, 'w').write('Done: I called the broker.')")
    )
    reply = cc.run_codex(cc.parse_spec(MODEL), "q")
    assert reply.startswith("Done: I called the broker.") and reply.endswith(cc.NO_ACTION_NOTICE)
