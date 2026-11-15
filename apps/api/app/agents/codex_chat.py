"""Chat mode on the owner's ChatGPT subscription through the Codex CLI (`SUPERVISOR_MODEL=codex:<model>[:<effort>]`).

A subscription is not an API key; the supported way to use it from a program is `codex exec` (non-interactive).
Design:
- One model call per message, and the model gets no tools. The project's facts are gathered first with the same
  read-only tools the other modes use (project state, ranking, cost scenarios, feasibility, evidence and the
  evidence check for the top-ranked properties), then one `codex exec` call gets a prompt holding the rules, the
  facts as JSON, the recent turns and the owner's message. Its last message is the reply.
- The model cannot act. The CLI runs with the read-only sandbox, the shell/exec/browser tools disabled, the user's
  Codex config and rules ignored, in an empty temporary directory, with `--ephemeral` and a hard timeout. Its
  environment holds only PATH, HOME, CODEX_HOME and the locale: no app setting, key or database URL. The prompt
  goes on standard input, so it never shows in the process list.
- Requests to act (call, email, text, buy, approve...) never reach the model: they get a fixed answer saying the
  action is requested and approved in the app, and that nothing was done (mode `deterministic:action_request`).
- Failure is said: a missing CLI, no sign-in, a timeout, a non-zero exit or an empty answer gives a reply that says
  the assistant's model could not be reached (mode `codex_cli_unavailable:...`). No fallback to another mode,
  no exception, and no CLI output in the reply or the log (only the failure kind and exit code are logged).
- No internal id reaches a person. The prompt gets `public_facts`: every listing is named by its address and suite
  (from the ranking's own explanation text), evidence by a prompt-local reference (E1, E2...), and every other
  record id is dropped. The record ids the route returns (`grounded_record_ids`) come from the raw facts, which stay
  in this process. As a second line of defence `scrub_ids` removes any UUID from the model's reply before the
  reply is stored or returned.
Privacy: the facts and the recent turns are sent to OpenAI through the owner's subscription; nothing else is.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time
from collections import deque
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from uuid import UUID

from app.contracts import ChatResponse
from app.core.config import ProviderMode, Settings
from app.core.logging import get_logger

from .backend_client import TOOL_ERROR_STATUS
from .tools import ToolContext, build_tools

log = get_logger(__name__)

PREFIX = "codex:"
EFFORTS = ("minimal", "low", "medium", "high", "xhigh")
_MODEL_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")

# Seconds before the CLI is killed (with its process group); the owner then reads that the model did not answer.
TIMEOUT_S = 90
# Properties (in rank order) whose feasibility, evidence and evidence check go into the facts.
TOP_PROPERTIES = 3
MAX_EVIDENCE_PER_PROPERTY = 20
MAX_FACTS_CHARS = 60_000
# Past exchanges (owner message + reply) kept per thread, in this process only, and sent with the next message.
HISTORY_TURNS = 4
MAX_HISTORY_CHARS = 1_500
# The CLI's own features that would let the model run commands, open pages or read or make images; all switched off.
# The CLI rejects an unknown feature name, so a renamed feature fails every message loudly instead of staying on.
DISABLED_FEATURES = (
    "shell_tool",
    "unified_exec",
    "browser_use",
    "computer_use",
    "in_app_browser",
    "view_image",
    "image_generation",
)
# Only these variables reach the CLI. HOME/CODEX_HOME locate its sign-in; nothing from the app's settings.
ENV_PASSTHROUGH = ("PATH", "HOME", "CODEX_HOME", "LANG", "LC_ALL")

SYSTEM_RULES = """You are the Second Site assistant. You help a business owner open their next location.
Rules, all binding:
- Answer only from the FACTS JSON below and the conversation. Do not use outside knowledge about these properties,
  prices, permits, laws or infrastructure. If the facts do not answer the question, say that you do not know.
- Where a fact is UNKNOWN (or missing), say it is unknown. Never treat UNKNOWN as meeting a requirement.
- Historical permits do not prove current capacity. Evidence classes (historical, visual, measured, owner-reported,
  professionally verified, derived, unknown, conflicting) stay separate; say which class a fact is.
- Money in the facts is integer US cents. Give amounts exactly as the facts give them, converted to dollars and
  cents only by moving the decimal point (123456 cents = $1,234.56). Do not estimate or round new figures.
- If data_notice says the data is fixture (demo) data, say so whenever you cite a listing, permit or price.
- You cannot contact anyone, place calls, send messages, buy anything or approve anything. Never say that you did.
  Outreach and purchases are requested and approved by the owner in the app.
- Reply in plain text, briefly (at most about 200 words). No tables, no headings. Name a listing by its address
  and suite, as its "listing" field gives them, and say its rank when that helps. Never write an id, a database
  key or a reference code such as E3: say the evidence's class and where it comes from instead."""

ACTION_REPLY = (
    "I can't do that from chat: I can't contact anyone, place calls, send messages, buy anything or approve "
    "anything. Outreach and purchases are requested and approved by you in the app ({web}). Nothing was sent, "
    "called, bought or approved. Ask me a question about your project and I'll answer from its records."
)
UNAVAILABLE_REPLY = "The assistant's model could not be reached: {why}. Nothing was done. Try again later."
# The model has no tools, so a reply that says it acted is false; it is passed on with this notice appended.
NO_ACTION_NOTICE = "(Note: this assistant cannot act. Nothing was sent, called, bought, booked or approved.)"
_CLAIM_RE = re.compile(
    r"\bI(?:'ve| have| just)?\s+(?:called|phoned|rang|dialed|e-?mailed|texted|messaged|contacted|reached out|"
    r"got in touch|let|told|informed|sent|submitted|put in|placed|booked|reserved|scheduled|bought|purchased|"
    r"paid|wired|ordered|approved|signed|accepted|locked|confirmed|made an offer)\b",
    re.IGNORECASE,
)
MAX_REPLY_CHARS = 4_000  # the reply also goes out by text message (Photon); the rules ask for ~200 words

# Messages that ask the assistant to act. They get ACTION_REPLY and never reach the model.
_ACTION_RE = re.compile(
    r"\b(call|phone|ring|e-?mail|sms|text (him|her|them|the|my|our|a|an)|message (him|her|them|the)|contact|"
    r"reach out|outreach|send|buy|purchase|pay|place an order|order (the|a|an|some)|approve|book|negotiate|"
    r"sign (the|a)|schedule (a|the))\b",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class CodexSpec:
    model: str
    effort: str | None

    @property
    def label(self) -> str:
        return f"{self.model}:{self.effort}" if self.effort else self.model


class CodexUnavailable(Exception):
    """The CLI could not give an answer. `why` is fixed text for the owner; it never carries CLI output."""

    def __init__(self, kind: str, why: str, exit_code: int | None = None):
        super().__init__(kind)
        self.kind = kind
        self.why = why
        self.exit_code = exit_code


def is_codex_model(model: object) -> bool:
    return isinstance(model, str) and model.strip().lower().startswith(PREFIX)


def parse_spec(model: str) -> CodexSpec:
    """`codex:<model>[:<effort>]`. Raises ValueError for a bad model name or an unknown effort."""
    rest = model.strip()[len(PREFIX) :]
    name, _, effort = rest.partition(":")
    if not _MODEL_RE.fullmatch(name):
        raise ValueError("SUPERVISOR_MODEL codex: model name is missing or has unexpected characters")
    effort = effort.strip().lower() or None
    if effort is not None and effort not in EFFORTS:
        raise ValueError(f"SUPERVISOR_MODEL codex: effort must be one of {', '.join(EFFORTS)}")
    return CodexSpec(name, effort)


def is_action_request(message: str) -> bool:
    return bool(_ACTION_RE.search(message))


# ---- conversation memory (process-local) ---------------------------------------------------------------------
_HISTORY: dict[str, deque[tuple[str, str]]] = {}
_HISTORY_LOCK = threading.Lock()


def _history(thread: str) -> list[tuple[str, str]]:
    with _HISTORY_LOCK:
        return list(_HISTORY.get(thread, ()))


def _remember(thread: str, message: str, reply: str) -> None:
    with _HISTORY_LOCK:
        turns = _HISTORY.setdefault(thread, deque(maxlen=HISTORY_TURNS))
        turns.append((message[:MAX_HISTORY_CHARS], reply[:MAX_HISTORY_CHARS]))


def clear_history() -> None:
    with _HISTORY_LOCK:
        _HISTORY.clear()


# ---- facts ---------------------------------------------------------------------------------------------------
def _call(tools: dict[str, Any], name: str, args: dict[str, Any] | None = None) -> Any:
    """The tool's JSON, or an error marker. A tool that raises (in-process mode) gives only its exception type."""
    try:
        text = tools[name].invoke(args or {})
    except Exception as exc:  # noqa: BLE001 - recorded as unavailable facts, never shown as data
        return {"status": TOOL_ERROR_STATUS, "error_code": type(exc).__name__}
    try:
        return json.loads(text)
    except (TypeError, ValueError):
        return {"status": TOOL_ERROR_STATUS, "error_code": "BAD_TOOL_ANSWER"}


def _data_notice(settings: Settings) -> str:
    fixture = [
        name
        for name, mode in (
            ("property listings", settings.property_provider_mode),
            ("permits", settings.permit_provider_mode),
        )
        if mode == ProviderMode.FIXTURE
    ]
    if not fixture:
        return "Listings and permits come from the configured providers; each fact keeps its source id."
    return (
        f"{' and '.join(fixture).capitalize()} in these facts are fixture (demo) data, not live records. "
        "Say so when citing them."
    )


def gather_facts(ctx: ToolContext) -> tuple[dict[str, Any], list[UUID]]:
    """Read-only facts for the prompt and the ids of the records they came from. Never runs the analysis."""
    tools = {t.name: t for t in build_tools(ctx)}
    ids: list[UUID] = [ctx.project_id]
    facts: dict[str, Any] = {
        "data_notice": _data_notice(ctx.c.settings),
        "money_unit": "integer US cents",
        "project_state": _call(tools, "get_project_state"),
        "ranking": _call(tools, "get_ranking"),
        "cost_scenarios": _call(tools, "get_cost_scenarios"),
    }
    ranking = facts["ranking"]
    if isinstance(ranking, dict) and ranking.get("status") == "NO_RANKING":
        facts["ranking_note"] = "No ranking exists yet; the owner can run the property search in the app."
    ranked = ranking.get("ranked") if isinstance(ranking, dict) else None
    if isinstance(ranking, dict) and isinstance(ranking.get("ranking_id"), str):
        ids.append(UUID(ranking["ranking_id"]))
    details = []
    for r in (ranked if isinstance(ranked, list) else [])[:TOP_PROPERTIES]:
        if not isinstance(r, dict) or not isinstance(r.get("property_id"), str):
            continue
        pid = r["property_id"]
        evidence = _call(tools, "get_property_evidence", {"property_id": pid})
        if isinstance(evidence, list):
            evidence = evidence[:MAX_EVIDENCE_PER_PROPERTY]
            ids += [UUID(e["id"]) for e in evidence if isinstance(e, dict) and isinstance(e.get("id"), str)]
        details.append(
            {
                "property_id": pid,
                "rank": r.get("rank"),
                "feasibility": _call(tools, "get_feasibility", {"property_id": pid}),
                "evidence": evidence,
                "evidence_check": _call(tools, "validate_evidence", {"property_id": pid}),
            }
        )
        ids.append(UUID(pid))
    facts["top_properties"] = details
    if len(json.dumps(facts, default=str)) > MAX_FACTS_CHARS:
        for d in details:
            d["evidence"] = "omitted: too long for one message"
        facts["truncated"] = True
    ids += [i for i in ctx.touched_records if i not in ids]
    return facts, list(dict.fromkeys(ids))


# ---- no internal id in the prompt or the reply ------------------------------------------------------------------
_UUID = r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
_UUID_RE = re.compile(_UUID)
_ORDINALS = ("first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth", "tenth")
UNRANKED_LISTING = "a listing that is not in the ranking"


def _is_id(value: object) -> bool:
    return isinstance(value, str) and _UUID_RE.fullmatch(value) is not None


def listing_labels(facts: dict[str, Any]) -> dict[str, str]:
    """Property id -> the name a person knows it by: its address and suite, as the ranking's explanation starts
    with them ("<ADDRESS, SUITE>: ..."). Listings that share an address are told apart by their place in the
    ranking ("the first listing at ..."); a listing without an address is "the listing ranked N". This map stays
    in the process: it is how a name in the prompt ties back to a record."""
    ranking = facts.get("ranking")
    ranked = ranking.get("ranked") if isinstance(ranking, dict) else None
    explanations = ranking.get("explanations") if isinstance(ranking, dict) else None
    named: list[tuple[str, Any, str]] = []
    for r in ranked if isinstance(ranked, list) else []:
        if not isinstance(r, dict) or not _is_id(r.get("property_id")):
            continue
        pid = r["property_id"].lower()
        text = explanations.get(r["property_id"]) if isinstance(explanations, dict) else None
        address = text.partition(":")[0].strip() if isinstance(text, str) and ":" in text else ""
        if _UUID_RE.search(address) or len(address) > 120:
            address = ""
        named.append((pid, r.get("rank"), address))
    count: dict[str, int] = {}
    for _pid, _rank, address in named:
        count[address] = count.get(address, 0) + 1
    seen: dict[str, int] = {}
    labels: dict[str, str] = {}
    for position, (pid, rank, address) in enumerate(named, start=1):
        if not address:
            label = f"the listing ranked {rank if isinstance(rank, int) else position}"
        elif count[address] == 1:
            label = address
        else:
            n = seen[address] = seen.get(address, 0) + 1
            nth = _ORDINALS[n - 1] if n <= len(_ORDINALS) else f"number {n}"
            label = f"the {nth} listing at {address}"
        while label in labels.values():  # never two listings under one name
            label += " (another)"
        labels[pid] = label
    return labels


def public_facts(facts: dict[str, Any]) -> dict[str, Any]:
    """The facts as the model gets them: the same records, with no UUID anywhere. `property_id` becomes `listing`
    (see `listing_labels`), an evidence id becomes a reference local to this prompt (`ref`, `evidence_refs`: E1,
    E2...), a map keyed by property id is keyed by the listing's name, and every other record id is left out."""
    labels = listing_labels(facts)
    refs: dict[str, str] = {}

    def ref(evidence_id: str) -> str:
        return refs.setdefault(evidence_id.lower(), f"E{len(refs) + 1}")

    def name(m: re.Match[str]) -> str:
        return labels.get(m.group().lower(), m.group())

    def walk(o: Any) -> Any:
        if isinstance(o, dict):
            out: dict[str, Any] = {}
            for k, v in o.items():
                key = str(k)
                if _is_id(key):  # a map keyed by property id
                    key = labels.get(key.lower(), "")
                    if key:
                        out[key] = walk(v)
                elif key.endswith("property_id") and _is_id(v):  # property_id, selected_property_id
                    out[key[: -len("property_id")] + "listing"] = labels.get(v.lower(), UNRANKED_LISTING)
                elif key in ("id", "evidence_id") and _is_id(v):
                    out["ref"] = ref(v)
                elif key == "evidence_ids" and isinstance(v, list):
                    out["evidence_refs"] = [ref(x) for x in v if _is_id(x)]
                elif _is_id(v) or (isinstance(v, list) and v and all(_is_id(x) for x in v)):
                    continue  # any other record id (project, owner, ranking, scenario...): not for the prompt
                else:
                    out[key] = walk(v)
            return out
        if isinstance(o, list):
            return [walk(x) for x in o if not _is_id(x)]
        if isinstance(o, str) and _UUID_RE.search(o):
            return scrub_ids(_UUID_RE.sub(name, o))
        return o

    return walk(facts)


_MARK = "\x00"  # stands for a removed id while the words around it are tidied
_SHORT_ID = r"(?<![0-9A-Za-z-])[0-9a-fA-F]{8}-(?:…|\.\.\.)"  # an id cut short: "c96c80ae-…"
_ID_RE = re.compile(rf"{_UUID}|{_SHORT_ID}")
_URL_WITH_ID_RE = re.compile(
    rf"(?:https?://|www\.)[^\s<>()\[\]]*?{_UUID}[^\s<>()\[\]]*?(?=[.,;:!?]*(?:[\s<>()\[\]]|$))", re.IGNORECASE
)
_ID_LIST_RE = re.compile(rf"{_MARK}(?:\s*(?:[,;/&]|\band\b|\bor\b)?\s*{_MARK})+")
# The words that only introduce an id: "(see) (the) property", "ranking id:", "id =", "evidence ids".
_ID_LABEL_RE = re.compile(
    r"(?P<kept>(?:\b(?:with|under|in|from|see|per|by|via|for)\s+)?(?:\b(?:the|its|their|this|that)\s+)?"
    r"(?:\b(?:property|properties|ranking|rankings|evidence|project|listing|record|approval|scenario|source))?)"
    rf"(?P<glue>[\s_-]*(?:(?:ids?|uuids?|keys?)\b)?\s*(?:[:=#]|\bis\b|\bare\b)?\s*){_MARK}",
    re.IGNORECASE,
)
_NOUN_END_RE = re.compile(
    r"(?:property|ranking|evidence|project|listing|record|approval|scenario)$", re.IGNORECASE
)


def _label(m: re.Match[str]) -> str:
    """ "Property <id> ranks first" keeps its noun ("Property ranks first"); "(property <id>)", "id: <id>" do not."""
    rest = m.string[m.end() :].lstrip(" \t")
    plain_glue = not re.search(r"[:=#]|\bids?\b|\bis\b|\bare\b", m.group("glue"), re.IGNORECASE)
    if plain_glue and _NOUN_END_RE.search(m.group("kept")) and rest[:1].isalnum():
        return m.group("kept") + " " + _MARK
    return _MARK


def _gap(m: re.Match[str]) -> str:
    before = m.string[m.start() - 1] if m.start() else ""
    after = m.string[m.end()] if m.end() < len(m.string) else ""
    return "" if before in ("", "\n", "(", "[", "{") or after in ("", "\n", *".,;:!?)]}") else " "


def scrub_ids(text: str) -> str:
    """Remove every internal id (a UUID in any case, a UUID cut short, a URL that carries one) from a reply, with
    the words, brackets and separators that only carried it. A reply without an id is returned unchanged, byte for
    byte; a reply that was nothing but ids comes back empty (the caller then says the model gave no answer)."""
    if not _ID_RE.search(text):
        return text
    t = text.replace(_MARK, "")
    t = _URL_WITH_ID_RE.sub(_MARK, t)
    t = _ID_RE.sub(_MARK, t)
    t = _ID_LIST_RE.sub(_MARK, t)
    t = _ID_LABEL_RE.sub(_label, t)
    t = _ID_LIST_RE.sub(_MARK, t)
    t = re.sub(rf"[ \t]*[(\[{{]\s*{_MARK}\s*[)\]}}]", "", t)  # brackets that held only ids
    t = re.sub(rf"(?m)^[ \t]*(?:[-*•][ \t]*)?{_MARK}[ \t]*[.,;]?[ \t]*(?:\n|\Z)", "", t)  # a line of only ids
    t = re.sub(rf"(?<=[.!?])[ \t]*{_MARK}[ \t]*[.!?;,]+", "", t)  # a sentence of only ids
    t = re.sub(rf"\s*[,;:–—-]\s*{_MARK}(?=\s*(?:[.!?)\]}}]|\n|\Z))", "", t)  # "..., <id>." and "(...; <id>)"
    t = re.sub(rf"{_MARK}\s*[,;:]\s*", "", t)  # "(<id>; ...)"
    t = re.sub(rf"[ \t]*{_MARK}[ \t]*", _gap, t).strip()
    return t if re.search(r"[0-9A-Za-z]", t) else ""


def build_prompt(facts: dict[str, Any], history: list[tuple[str, str]], message: str) -> str:
    parts = [SYSTEM_RULES, "", "FACTS (JSON):", json.dumps(facts, default=str, separators=(",", ":")), ""]
    if history:
        parts.append("RECENT CONVERSATION (oldest first):")
        for owner, reply in history:
            parts += [f"Owner: {owner}", f"Assistant: {reply}"]
        parts.append("")
    parts += ["OWNER'S MESSAGE:", message, "", "Write the assistant's reply."]
    return "\n".join(parts)


# ---- the CLI -------------------------------------------------------------------------------------------------
def command(executable: str, spec: CodexSpec, workdir: str, out_file: str) -> list[str]:
    argv = [executable, "exec", "-m", spec.model]
    if spec.effort:
        argv += ["-c", f'model_reasoning_effort="{spec.effort}"']
    argv += ["-c", 'web_search="disabled"']  # the CLI rejects an unknown value, so this setting is checked
    argv += ["-s", "read-only", "-C", workdir, "--skip-git-repo-check", "--ephemeral"]
    argv += ["--ignore-user-config", "--ignore-rules", "--color", "never"]
    for feature in DISABLED_FEATURES:
        argv += ["--disable", feature]
    argv += ["-o", out_file, "-"]  # "-": the prompt is read from standard input
    return argv


def _cli_env() -> dict[str, str]:
    return {k: os.environ[k] for k in ENV_PASSTHROUGH if os.environ.get(k)}


_SIGN_IN_RE = re.compile(r"log ?in|sign(ed)? ?in|unauthori[sz]ed|\b401\b|auth", re.IGNORECASE)


def run_codex(spec: CodexSpec, prompt: str, timeout_s: float | None = None) -> str:
    """The model's last message, or CodexUnavailable. Output of the CLI is never logged or returned on failure."""
    exe = shutil.which("codex")
    if exe is None:
        raise CodexUnavailable("missing", "the Codex CLI is not installed on the server")
    with tempfile.TemporaryDirectory(prefix="ss-codex-") as base:
        workdir = Path(base, "work")
        workdir.mkdir()  # empty: the model has no files to look at
        out_file = Path(base, "last-message.txt")  # outside the working directory
        try:
            proc = subprocess.Popen(  # noqa: S603 - fixed argv, no shell
                command(exe, spec, str(workdir), str(out_file)),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=workdir,
                env=_cli_env(),
                encoding="utf-8",
                errors="replace",  # invalid UTF-8 from the CLI must not raise (that would be a 500)
                start_new_session=True,
            )
        except OSError:
            raise CodexUnavailable("start", "the Codex CLI could not be started") from None
        try:
            _stdout, stderr = proc.communicate(prompt, timeout=timeout_s or TIMEOUT_S)
        except subprocess.TimeoutExpired:
            _kill_group(proc)
            raise CodexUnavailable(
                "timeout", f"the model did not answer within {timeout_s or TIMEOUT_S:g} seconds"
            ) from None
        if proc.returncode != 0:
            if _SIGN_IN_RE.search(stderr or ""):
                raise CodexUnavailable(
                    "not_signed_in",
                    "the Codex CLI is not signed in or its sign-in was refused",
                    proc.returncode,
                )
            raise CodexUnavailable(
                "exit", f"the Codex CLI stopped with exit code {proc.returncode}", proc.returncode
            )
        answer = out_file.read_text(encoding="utf-8", errors="replace").strip() if out_file.exists() else ""
    answer = scrub_ids(answer)  # before the reply is stored, returned or cut: no internal id reaches a person
    if not answer:
        raise CodexUnavailable("empty", "the model returned an empty answer")
    if len(answer) > MAX_REPLY_CHARS:
        answer = answer[:MAX_REPLY_CHARS].rstrip() + " [reply cut: too long]"
    if _CLAIM_RE.search(answer):
        answer = f"{answer}\n\n{NO_ACTION_NOTICE}"
    return answer


def _kill_group(proc: subprocess.Popen) -> None:
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        proc.kill()
    proc.communicate()


# ---- the chat ------------------------------------------------------------------------------------------------
def codex_chat(ctx: ToolContext, message: str, model: str, thread_id: str) -> ChatResponse:
    try:
        spec = parse_spec(model)
    except ValueError as exc:
        log.warning("codex chat misconfigured: %s", exc)
        return ChatResponse(
            reply=UNAVAILABLE_REPLY.format(why="SUPERVISOR_MODEL has an invalid codex: setting"),
            mode="codex_cli_unavailable:invalid_setting",
        )
    if is_action_request(message):
        return ChatResponse(
            reply=ACTION_REPLY.format(web=ctx.c.settings.public_web_url), mode="deterministic:action_request"
        )
    t0 = time.monotonic()
    raw_facts, ids = gather_facts(ctx)  # the raw facts and their record ids stay in this process
    facts = public_facts(raw_facts)  # what the model gets: listings by address, no UUID
    history = _history(thread_id)
    prompt = build_prompt(facts, history, message)
    t1 = time.monotonic()
    reply = ""
    try:
        reply = run_codex(spec, prompt)
    except CodexUnavailable as exc:
        log.warning("codex chat unavailable: kind=%s exit_code=%s", exc.kind, exc.exit_code)
        _log_timing(spec, t0, t1, prompt, facts, history, reply)
        return ChatResponse(
            reply=UNAVAILABLE_REPLY.format(why=exc.why),
            mode=f"codex_cli_unavailable:{spec.label}",
            grounded_record_ids=[],
        )
    _log_timing(spec, t0, t1, prompt, facts, history, reply)
    _remember(thread_id, message, reply)
    return ChatResponse(reply=reply, mode=f"codex_cli:{spec.label}", grounded_record_ids=ids)


def _log_timing(
    spec: CodexSpec, t0: float, t1: float, prompt: str, facts: dict[str, Any], history: list, reply: str
):
    """Numbers and the mode label only: never the prompt, facts, message, reply, CLI output or an id."""
    log.info(
        "codex chat timing: facts_s=%.2f cli_s=%.2f prompt_chars=%d facts_chars=%d history_turns=%d reply_chars=%d mode=%s",
        t1 - t0,
        time.monotonic() - t1,
        len(prompt),
        len(json.dumps(facts, default=str, separators=(",", ":"))),
        len(history),
        len(reply),
        spec.label,
    )
