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
        "-c", 'web_search="disabled"',
        "-s", "read-only", "-C", "/tmp/w", "--skip-git-repo-check", "--ephemeral",
        "--ignore-user-config", "--ignore-rules", "--color", "never",
        "--disable", "shell_tool", "--disable", "unified_exec", "--disable", "browser_use",
        "--disable", "computer_use", "--disable", "in_app_browser",
        "--disable", "view_image", "--disable", "image_generation",
        "-o", "/tmp/o.txt", "-",
    ]  # fmt: skip
    no_effort = cc.command("codex", cc.CodexSpec("m", None), "/w", "/o")
    assert "model_reasoning_effort" not in " ".join(no_effort)
    assert (
        no_effort[no_effort.index("-c") + 1] == 'web_search="disabled"'
    )  # web search is off with or without effort


# ---- the prompt (assistant-speed-1: compact facts, same rules) ------------------------------------------------
# SYSTEM_RULES as reviewed, but for its last rule (assistant-ids-1): "Cite property ids..." is gone and the
# form rule now says to name a listing by its address and never to write an id.
RULES_SHA256 = "8dfc9eed6dad14355de5504d734255698955d997ace8659149a6b72d4283aff6"


def test_prompt_keeps_the_rules_and_every_fact_in_compact_json():
    import hashlib

    assert hashlib.sha256(cc.SYSTEM_RULES.encode()).hexdigest() == RULES_SHA256
    assert "Cite property ids" not in cc.SYSTEM_RULES and "Never write an id" in cc.SYSTEM_RULES
    facts = {"data_notice": "fixture", "money_unit": "integer US cents", "ranking": {"ranked": [{"rank": 1}]},
             "top_properties": [{"property_id": "p1", "evidence": [{"id": "e1", "class": "UNKNOWN"}]}]}  # fmt: skip
    prompt = cc.build_prompt(facts, [("q1", "a1")], "Which ranks first?")
    lines = prompt.split("\n")
    assert prompt.startswith(cc.SYSTEM_RULES + "\n\nFACTS (JSON):\n")
    facts_line = lines[lines.index("FACTS (JSON):") + 1]
    assert json.loads(facts_line) == facts  # every field kept, nothing dropped
    assert facts_line == json.dumps(facts, separators=(",", ":"))  # one line, no indentation or spaces
    # The fixed parts come first and the changing message last.
    assert prompt.index("FACTS") < prompt.index("RECENT CONVERSATION") < prompt.index("OWNER'S MESSAGE")
    assert prompt.endswith("OWNER'S MESSAGE:\nWhich ranks first?\n\nWrite the assistant's reply.")


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
    # No internal id in the prompt: the listing goes by its address; the record ids come back beside the reply.
    assert not cc._UUID_RE.search(stdin)
    address = ranking.explanations[str(first)].partition(":")[0]
    assert address and f'"listing":"{address}"' in stdin
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


@pytest.mark.parametrize("exit_code", [0, 1])
def test_timing_line_has_numbers_only(ranked_ctx, container, tmp_path, monkeypatch, caplog, exit_code):
    """Orchestrator-requested timing log (assistant-speed-1 review): numbers and the mode label, no text or id."""
    bin_dir, _ = _fake_cli(tmp_path, exit_code=exit_code)
    _use(monkeypatch, bin_dir)
    caplog.set_level(logging.INFO)
    message = "Which space ranks first, zebra-probe?"
    run_chat(ranked_ctx, message, container.settings, model=MODEL)
    recs = [r.getMessage() for r in caplog.records if r.getMessage().startswith("codex chat timing:")]
    assert len(recs) == 1
    line = recs[0]
    for field in (
        "facts_s=",
        "cli_s=",
        "prompt_chars=",
        "facts_chars=",
        "history_turns=0",
        "mode=gpt-6.1-terra:low",
    ):
        assert field in line
    assert ("reply_chars=0" in line) == (exit_code != 0)
    assert "zebra-probe" not in line and "Which space" not in line and CANNED not in line
    assert str(ranked_ctx.project_id) not in line


# ---- no internal id reaches a person (assistant-ids-1) ---------------------------------------------------------
P1, P2, P3, P4 = (f"{n}{n}{n}{n}{n}{n}{n}{n}-0000-4000-8000-00000000000{n}" for n in "1234")
RK, EV1, EV2, OTHER = (f"{c}{c}{c}{c}{c}{c}{c}{c}-AAAA-4AAA-8AAA-AAAAAAAAAAAA".lower() for c in "abcd")


def _synthetic_facts():
    return {
        "data_notice": "fixture",
        "project_state": {
            "project": {"project_id": OTHER, "owner_id": OTHER, "project_name": "Studio two"},
            "selected_property_id": P2,
        },
        "ranking": {
            "ranking_id": RK,
            "ranked": [
                {"rank": i, "property_id": p, "total_score": 0.5} for i, p in enumerate((P1, P2, P3, P4), 1)
            ],
            "explanations": {
                P1: "12 EXAMPLE ST, 100: expected opening cost $1.00.",
                P2: "12 EXAMPLE ST: expected opening cost $2.00.",
                P3: f"12 EXAMPLE ST: Lower rent than {P1}.",
            },
        },
        "cost_scenarios": [{"property_id": P2, "monthly": 100}, {"property_id": OTHER, "monthly": 5}],
        "top_properties": [
            {
                "property_id": P1,
                "rank": 1,
                "feasibility": [
                    {"requirement": "electrical", "status": "UNKNOWN", "evidence_ids": [EV2, EV1]}
                ],
                "evidence": [{"id": EV1, "class": "HISTORICAL", "source": "permit 18041-90000-12345"}],
                "evidence_check": {"problems": []},
            }
        ],
    }


def test_public_facts_name_listings_by_address_and_carry_no_id():
    raw = _synthetic_facts()
    labels = cc.listing_labels(raw)
    # Suite tells two listings of one building apart; two with the same address go by their place in the ranking;
    # a listing whose address is not known goes by its rank.
    assert labels == {
        P1: "12 EXAMPLE ST, 100",
        P2: "the first listing at 12 EXAMPLE ST",
        P3: "the second listing at 12 EXAMPLE ST",
        P4: "the listing ranked 4",
    }
    assert len(set(labels.values())) == 4
    facts = cc.public_facts(raw)
    text = json.dumps(facts)
    assert not cc._UUID_RE.search(text)
    assert raw == _synthetic_facts()  # the raw facts (and their ids) are not changed
    assert [r["listing"] for r in facts["ranking"]["ranked"]] == list(labels.values())
    assert [r["rank"] for r in facts["ranking"]["ranked"]] == [1, 2, 3, 4]
    assert "ranking_id" not in facts["ranking"]
    assert facts["project_state"] == {
        "project": {"project_name": "Studio two"},
        "selected_listing": labels[P2],
    }
    assert set(facts["ranking"]["explanations"]) == set(list(labels.values())[:3])
    assert (
        facts["ranking"]["explanations"][labels[P3]] == "12 EXAMPLE ST: Lower rent than 12 EXAMPLE ST, 100."
    )
    assert facts["cost_scenarios"] == [
        {"listing": labels[P2], "monthly": 100},
        {"listing": cc.UNRANKED_LISTING, "monthly": 5},
    ]
    top = facts["top_properties"][0]
    assert top["listing"] == "12 EXAMPLE ST, 100" and top["rank"] == 1
    # Evidence keeps its link to the assessment by a reference local to the prompt; everything else is kept as is.
    assert top["feasibility"] == [
        {"requirement": "electrical", "status": "UNKNOWN", "evidence_refs": ["E1", "E2"]}
    ]
    assert top["evidence"] == [{"ref": "E2", "class": "HISTORICAL", "source": "permit 18041-90000-12345"}]
    assert top["evidence_check"] == {"problems": []} and facts["data_notice"] == "fixture"


U1, U2 = "c96c80ae-1111-4222-8333-444455556666", "B6F0456F-AAAA-4BBB-8CCC-DDDDEEEEFFFF"
UNTOUCHED = "No id here:  two spaces, () brackets, permit 18041-90000-12345, 2026-11-15, deadbeef-cafe."


@pytest.mark.parametrize(
    ("reply", "clean"),
    [
        # 1 the reply of the demo sweep: a parenthetical that only carried ids
        (
            f"The top listing is 5757 W Century Blvd (property {U1}; ranking {U2}).",
            "The top listing is 5757 W Century Blvd.",
        ),
        # 2 a clause that only carried an id
        (
            f"The first listing is 122 E 7th St, Suite 1207, id: {U1}.",
            "The first listing is 122 E 7th St, Suite 1207.",
        ),
        # 3 upper case
        (f"It ranks first (ranking {U2}).", "It ranks first."),
        # 4 inside a URL: the link goes with it
        (
            f"You can see it in the app (https://app.example/properties/{U1}/layout?x=1).",
            "You can see it in the app.",
        ),
        # 5 nothing but an id
        (U1, ""),
        (f"property_id={U1}", ""),
        # 6 in the middle of a sentence: the noun stays, the id goes
        (f"Property {U1} ranks first on cost.", "Property ranks first on cost."),
        # 7 a list of ids in square brackets
        (
            f"The electrical service is unknown [evidence {U1}, {U1} and {U2}].",
            "The electrical service is unknown.",
        ),
        # 8 brackets that hold other words too
        (f"This is demo data (fixture; property {U1}).", "This is demo data (fixture)."),
        # 9 a line, a bullet and a sentence of their own
        (f"Line one.\nProperty ID: {U1}\nLine three.", "Line one.\nLine three."),
        (f"- 5757 W Century Blvd\n- Ranking id: {U2}", "- 5757 W Century Blvd"),
        (
            f"The top listing is 5757 W Century Blvd. Its property id is {U1}.",
            "The top listing is 5757 W Century Blvd.",
        ),
        # 10 an id cut short, as a model may print it
        (
            "5757 W Century Blvd (property c96c80ae-…; ranking b6f0456f-...) ranks first.",
            "5757 W Century Blvd ranks first.",
        ),
        # review: an id cut after a later group; an id between two clauses never joins their words; a bullet's lead
        (
            "5757 W Century Blvd (property c96c80ae-1111-…; ranking b6f0456f-aaaa-4bbb-...) ranks first.",
            "5757 W Century Blvd ranks first.",
        ),
        (f"Do not pick {U1}; pick 5757 W Century Blvd.", "Do not pick; pick 5757 W Century Blvd."),
        (f"- {U1}: 5757 W Century Blvd", "- 5757 W Century Blvd"),
        (f"It ranks first. {U1}: see the app.", "It ranks first. see the app."),
        # and a reply without an id is not touched at all
        (UNTOUCHED, UNTOUCHED),
    ],
)
def test_scrub_ids(reply, clean):
    out = cc.scrub_ids(reply)
    assert out == clean
    assert not cc._ID_RE.search(out) and "  " not in out.replace(UNTOUCHED, "")
    for leftover in ("()", "[]", "( ", " )", " .", " ,", "property.", "property)", "id:", ".."):
        assert leftover not in out.replace(UNTOUCHED, "")
    assert cc.scrub_ids(out) == out


def test_reply_is_scrubbed_before_it_is_stored_or_returned(ranked_ctx, container, tmp_path, monkeypatch):
    bin_dir, record = _fake_cli(
        tmp_path, answer=f"The top listing is 12 Example St (property {U1}; ranking {U2})."
    )
    _use(monkeypatch, bin_dir)
    res = run_chat(ranked_ctx, "Which space ranks first?", container.settings, model=MODEL, thread_id="ids")
    assert res.reply == "The top listing is 12 Example St."  # still says which listing it is about
    assert res.mode == "codex_cli:gpt-6.1-terra:low" and ranked_ctx.project_id in res.grounded_record_ids
    run_chat(ranked_ctx, "And the second?", container.settings, model=MODEL, thread_id="ids")
    stdin = json.loads(record.read_text())["stdin"]
    assert "Assistant: The top listing is 12 Example St." in stdin and not cc._UUID_RE.search(stdin)


def test_two_listings_outside_the_ranking_do_not_share_a_name():
    raw = {
        "ranking": {"ranked": []},
        "cost_scenarios": [{"property_id": p, "monthly": 1} for p in (P1, P2, P1)],
    }
    names = [s["listing"] for s in cc.public_facts(raw)["cost_scenarios"]]
    assert names[0] == names[2] == cc.UNRANKED_LISTING and names[1] not in ("", names[0])


def test_a_huge_reply_is_scrubbed_in_bounded_time(ranked_ctx, container, tmp_path, monkeypatch):
    import time

    bin_dir, _ = _fake_cli(tmp_path, answer=f"Top: {U1}" + " " * 100_000 + U1)
    _use(monkeypatch, bin_dir)
    t0 = time.monotonic()
    res = run_chat(ranked_ctx, "Which space ranks first?", container.settings, model=MODEL)
    assert res.reply == "Top" and time.monotonic() - t0 < 20
    # the scrub itself, without the cut, on 100,000 characters
    for run in (" ", "-", "_", "\n", ", ", "http://a"):
        t0 = time.monotonic()
        assert not cc._ID_RE.search(cc.scrub_ids(U1 + run * (100_000 // len(run)) + U1))
        assert time.monotonic() - t0 < 20, repr(run)


def test_reply_that_is_only_an_id_is_an_empty_answer(ranked_ctx, container, tmp_path, monkeypatch):
    bin_dir, _ = _fake_cli(tmp_path, answer=f"(property {U1})")
    _use(monkeypatch, bin_dir)
    res = run_chat(ranked_ctx, "Which space ranks first?", container.settings, model=MODEL)
    assert "the model returned an empty answer" in res.reply and U1 not in res.reply
    assert res.mode == "codex_cli_unavailable:gpt-6.1-terra:low" and res.grounded_record_ids == []


def test_public_facts_are_smaller_and_complete_for_the_fixture_project(ranked_ctx):
    raw, ids = cc.gather_facts(ranked_ctx)
    facts = cc.public_facts(raw)
    before, after = (len(json.dumps(f, default=str, separators=(",", ":"))) for f in (raw, facts))
    assert after < before and not cc._UUID_RE.search(json.dumps(facts, default=str))
    labels = cc.listing_labels(raw)
    ranked = raw["ranking"]["ranked"]
    assert len(labels) == len(ranked) == len(set(labels.values())) and all(labels.values())
    # The record ids the route returns still come from the raw facts.
    assert {UUID(p["property_id"]) for p in raw["top_properties"]} <= set(ids)
    assert [p["listing"] for p in facts["top_properties"]] == [labels[r["property_id"]] for r in ranked[:3]]
    # Nothing but ids was dropped: every other key of every top property is still there.
    for pub, src in zip(facts["top_properties"], raw["top_properties"], strict=True):
        assert set(pub) == (set(src) - {"property_id"}) | {"listing"}
        assert len(pub["evidence"]) == len(src["evidence"]) and len(pub["feasibility"]) == len(
            src["feasibility"]
        )
    print(f"FACTS_CHARS before={before} after={after}")
