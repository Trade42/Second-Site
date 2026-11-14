"""Escalation policy: when the assistant must hand off to the owner (deterministic keyword rules).

The voice model may also call `escalate_to_owner`; this function is the server-side backstop.
"""

from __future__ import annotations

import re

from app.contracts import HandoffReason

RULES: list[tuple[HandoffReason, re.Pattern[str]]] = [
    (HandoffReason.GUARANTOR_REQUESTED, re.compile(r"\bguarant(or|ee)\b", re.I)),
    (
        HandoffReason.FINANCIAL_DETAILS,
        re.compile(r"\b(deposit|first month|wire|payment|bank|credit (check|score))\b", re.I),
    ),
    (
        HandoffReason.BINDING_COMMITMENT,
        re.compile(r"\b(sign|commit|lock (it|this) in|agree to|LOI|letter of intent)\b", re.I),
    ),
    (
        HandoffReason.SENSITIVE_PERSONAL_INFO,
        re.compile(r"\b(ssn|social security|date of birth|driver'?s license)\b", re.I),
    ),
    (
        HandoffReason.COUNTERPARTY_REQUESTED_OWNER,
        re.compile(r"\b(speak|talk) (to|with) (the )?(owner|you(r)? (boss|client))\b", re.I),
    ),
]


def detect_escalation(utterance: str) -> HandoffReason | None:
    for reason, pat in RULES:
        if pat.search(utterance):
            return reason
    return None
