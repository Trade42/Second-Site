"""One voice conversation with the assistant: `POST /api/v1/projects/{project_id}/voice/session`.

A deliberate addition: no voice route existed in the Python backend before this module (the only xAI code was the
call script builder in `integrations/xai_voice`, which this module does not use). The text assistant
(`codex_chat`) is unchanged; this module imports its facts, notices and claim pattern so both answer from the same
records under the same truth rules.

What the route does:
1. It gathers the project's facts exactly as the text assistant does (`codex_chat.gather_facts`: read-only tools,
   cut at `MAX_FACTS_CHARS`) and builds `session.instructions` from `VOICE_RULES` plus the compact facts JSON.
2. It mints one short-lived client secret at xAI (`POST /v1/realtime/client_secrets`, at most
   `SECRET_SECONDS` = 120 s) with the server's `XAI_API_KEY` in the `Authorization` header and nowhere else.
3. It returns the secret once, to the project's owner, with the `session.update` event the page must send, the
   disclosure (also as a `force_message` event the page sends first, spoken verbatim without the model), the label,
   the no-action notice and the claim pattern the page checks transcripts against.

The voice session has no tools at all (`session.tools` is an empty list): no function tools and none of the
provider's own tools (`web_search`, `x_search`, `file_search`, `mcp`). It cannot act and cannot look anything up
outside the facts.

What is sent where, plainly: the project's facts (the same JSON the text assistant sends to OpenAI) go to the
owner's browser in `session_update.session.instructions`, and from the browser to xAI when the page sends that
event. xAI is a second outside provider besides the text assistant's. The server itself sends xAI only the mint
request (`expires_after`), no project data. The secret, the key, the provider's answer body and its headers are
never logged or stored; a failed mint is logged with its kind and HTTP status only.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from typing import Any, NoReturn

import httpx

from app.contracts import VoiceSessionResponse
from app.core.config import Settings
from app.core.errors import AppError
from app.core.logging import get_logger

from .codex_chat import _CLAIM_RE, MAX_FACTS_CHARS, NO_ACTION_NOTICE, gather_facts
from .tools import ToolContext

log = get_logger(__name__)

PROVIDER = "xai"
MODEL = "grok-voice-latest"
MINT_URL = "https://api.x.ai/v1/realtime/client_secrets"
WEBSOCKET_URL = f"wss://api.x.ai/v1/realtime?model={MODEL}"
SECRET_SECONDS = 120  # the brief's ceiling; the provider allows up to 3600
MINT_TIMEOUT_S = 10.0
VOICE = "eve"
AUDIO_FORMAT = {"type": "audio/pcm", "rate": 24000}

DISCLOSURE = (
    "This is the Second Site voice assistant. It is automated, not a person. It answers only from this "
    "project's records, and it cannot contact anyone or buy anything."
)
LABEL = (
    "Voice assistant: automated, answers from this project's records, cannot contact anyone or buy anything."
)

# The truth rules of codex_chat.SYSTEM_RULES, word for word (a test checks each one); only the form rules differ.
VOICE_RULES = """You are the Second Site assistant. You help a business owner open their next location.
You are speaking with the owner in a live voice conversation. You are an automated assistant, not a person.
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
- You have no tools. You cannot look anything up; the facts below are all you know about this project.
Speaking rules:
- Speak in short, plain sentences, at most about 60 words a turn. No tables, no lists, no headings, no markdown.
- Name a property by its address from the facts, never by reading out an id. Do not read out ids or long numbers
  other than money.
- Say dollar amounts the way a person says them aloud, with the exact cents from the facts.
- If the owner asks you to call, message, buy, book or approve something, say that you cannot, that nothing was
  done, and that they can request it in the app."""


class VoiceNotConfigured(AppError):
    status_code = 503
    error_code = "VOICE_NOT_CONFIGURED"
    retryable = False


class VoiceProviderUnavailable(AppError):
    status_code = 502
    error_code = "PROVIDER_UNAVAILABLE"

    def __init__(self, kind: str, *, retryable: bool, status: int | None = None):
        super().__init__(
            "The voice provider could not start a session. Nothing was done.",
            details={"provider": PROVIDER, "kind": kind},
        )
        self.retryable = retryable
        self.kind = kind
        self.status = status


# Tests replace this with an httpx.MockTransport; None uses the real network.
_TRANSPORT: httpx.BaseTransport | None = None


def build_instructions(facts: dict[str, Any]) -> str:
    return "\n".join(
        [VOICE_RULES, "", "FACTS (JSON):", json.dumps(facts, default=str, separators=(",", ":"))]
    )


def bound_facts(facts: dict[str, Any]) -> dict[str, Any]:
    """gather_facts already drops evidence past MAX_FACTS_CHARS; if still over, the per-property details go too."""
    if len(json.dumps(facts, default=str, separators=(",", ":"))) <= MAX_FACTS_CHARS:
        return facts
    out = dict(facts)
    out["top_properties"] = "omitted: too long for one voice session"
    out["truncated"] = True
    return out


def session_update(instructions: str) -> dict[str, Any]:
    """The `session.update` event the page sends first (field names: xAI's session.update reference)."""
    return {
        "type": "session.update",
        "session": {
            "instructions": instructions,
            "voice": VOICE,
            "turn_detection": {"type": "server_vad"},
            "audio": {"input": {"format": dict(AUDIO_FORMAT)}, "output": {"format": dict(AUDIO_FORMAT)}},
            "tools": [],
        },
    }


def disclosure_event() -> dict[str, Any]:
    """xAI's `force_message`: speaks DISCLOSURE verbatim, without the model (voice-1, proof 3)."""
    return {
        "type": "conversation.item.create",
        "item": {
            "type": "force_message",
            "role": "assistant",
            "interruptible": False,
            "content": [{"type": "output_text", "text": DISCLOSURE}],
        },
    }


def mint_client_secret(settings: Settings) -> tuple[str, datetime]:
    """(secret, expiry) from xAI. Raises VoiceNotConfigured or VoiceProviderUnavailable; never logs a value."""
    key = settings.xai_api_key.get_secret_value().strip() if settings.xai_api_key else ""
    if not key:
        raise VoiceNotConfigured("Voice is not configured on this server (XAI_API_KEY is not set).")
    try:
        with httpx.Client(timeout=MINT_TIMEOUT_S, transport=_TRANSPORT, follow_redirects=False) as client:
            resp = client.post(
                MINT_URL,
                json={"expires_after": {"seconds": SECRET_SECONDS}},
                headers={"Authorization": f"Bearer {key}"},
            )
    except httpx.TimeoutException:
        _fail("timeout", True)
    except httpx.HTTPError:
        _fail("network", True)
    if resp.status_code != 200:
        _fail("http_status", resp.status_code == 429 or resp.status_code >= 500, resp.status_code)
    try:
        body = resp.json()
        value, expires = body["value"], body["expires_at"]
    except (ValueError, TypeError, KeyError):
        _fail("bad_answer", False, resp.status_code)
    if not isinstance(value, str) or not value or isinstance(expires, bool) or not isinstance(expires, int):
        _fail("bad_answer", False, resp.status_code)
    try:
        expires_at = datetime.fromtimestamp(expires, tz=UTC)
    except (OverflowError, OSError, ValueError):
        _fail("bad_answer", False, resp.status_code)
    return value, expires_at


def _fail(kind: str, retryable: bool, status: int | None = None) -> NoReturn:
    log.warning("voice session mint failed: kind=%s status=%s retryable=%s", kind, status, retryable)
    raise VoiceProviderUnavailable(kind, retryable=retryable, status=status) from None


def voice_session(ctx: ToolContext) -> VoiceSessionResponse:
    settings = ctx.c.settings
    if not (settings.xai_api_key and settings.xai_api_key.get_secret_value().strip()):
        raise VoiceNotConfigured("Voice is not configured on this server (XAI_API_KEY is not set).")
    facts, ids = gather_facts(ctx)
    facts = bound_facts(facts)
    instructions = build_instructions(facts)
    secret, expires_at = mint_client_secret(settings)
    log.info("voice session minted: instructions_chars=%d records=%d", len(instructions), len(ids))
    return VoiceSessionResponse(
        mode="assistant",
        provider=PROVIDER,
        websocket_url=WEBSOCKET_URL,
        client_secret=secret,
        expires_at=expires_at,
        session_update=session_update(instructions),
        disclosure=DISCLOSURE,
        disclosure_event=disclosure_event(),
        label=LABEL,
        no_action_notice=NO_ACTION_NOTICE,
        claim_pattern=_CLAIM_RE.pattern,
        data_notice=str(facts.get("data_notice", "")),
        grounded_record_ids=ids,
    )
