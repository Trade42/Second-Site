"""What the assistant asks a broker, and how answers become structured findings.

Questions are grounded in the property's open feasibility assessments (UNKNOWN / MAY_REQUIRE_WORK), so every call
exists to resolve a specific unknown. Extraction is deterministic and conservative: an answer that does not clearly
state a value yields a finding with value=None ("unclear"), never a guessed number.
"""

from __future__ import annotations

import re
from typing import Any

from app.contracts import AssessmentStatus, CallQuestion, InfrastructureAssessment, StructuredFinding

QUESTION_BANK: list[tuple[str, CallQuestion]] = [
    (
        "electrical.tenant_service",
        CallQuestion(
            question="How many amps is the electrical service to the suite's own panel, and roughly how much is in use?",
            claim_type="tenant_subpanel_amps",
        ),
    ),
    (
        "plumbing.treatment_sinks",
        CallQuestion(
            question="Is there existing plumbing inside the suite, like a sink or a floor drain?",
            claim_type="in_unit_plumbing",
        ),
    ),
    (
        "hvac.ventilation",
        CallQuestion(
            question="Could the HVAC be zoned so that separate small rooms each get supply and return air?",
            claim_type="hvac_zonable",
        ),
    ),
    (
        "space.min_area",
        CallQuestion(
            question="What is the usable square footage of the suite?", claim_type="usable_area_sqft"
        ),
    ),
]
AVAILABILITY = CallQuestion(
    question="When would the space be available to start a lease?", claim_type="available_from"
)
MAX_QUESTIONS = 4

YES = re.compile(
    r"\b(yes|yeah|yep|there is|there's|it has|it does|we have|definitely|absolutely|correct|sure)\b", re.I
)
NO = re.compile(
    r"\b(no|nope|none|not really|there isn'?t|doesn'?t|don'?t have|it's all open|no plumbing)\b", re.I
)
UNSURE = re.compile(
    r"\b(not sure|don'?t know|no idea|would have to check|have to check|unsure|i'?ll find out)\b", re.I
)
AMPS = re.compile(r"(\d{2,4})\s*(?:-|\s)?(?:amps?|amperes?|a\b)", re.I)
SQFT = re.compile(r"(\d[\d,]{2,6})\s*(?:usable\s*)?(?:sq(?:uare)?\.?\s*(?:ft|feet)|sf\b)", re.I)


def build_script(assessments: list[InfrastructureAssessment]) -> list[CallQuestion]:
    open_ids = {
        a.requirement_id.split(".", 1)[1]: a.requirement_id
        for a in assessments
        if a.status in (AssessmentStatus.UNKNOWN, AssessmentStatus.MAY_REQUIRE_WORK)
        and "." in a.requirement_id
    }
    script = [
        q.model_copy(update={"requirement_id": open_ids[suffix]})
        for suffix, q in QUESTION_BANK
        if suffix in open_ids
    ][: MAX_QUESTIONS - 1]
    return [*script, AVAILABILITY]


def extract(question: CallQuestion, answer: str) -> StructuredFinding:
    value: Any = None
    unit: str | None = None
    if not UNSURE.search(answer):
        if question.claim_type == "tenant_subpanel_amps":
            m = AMPS.search(answer)
            if m:
                value, unit = int(m.group(1)), "A"
        elif question.claim_type == "usable_area_sqft":
            m = SQFT.search(answer)
            if m:
                value, unit = int(m.group(1).replace(",", "")), "sqft"
        elif question.claim_type in ("in_unit_plumbing", "hvac_zonable"):
            yes, no = bool(YES.search(answer)), bool(NO.search(answer))
            if yes != no:
                value = yes
        else:
            value = answer.strip()[:200] or None
    return StructuredFinding(
        question=question.question,
        answer=answer,
        claim_type=question.claim_type,
        value=value,
        unit=unit,
        speaker="counterparty",
        classification="OWNER_REPORTED",
    )


def describe(f: StructuredFinding) -> str:
    if f.value is None:
        return f"{f.claim_type}: unclear"
    if isinstance(f.value, bool):
        return f"{f.claim_type}: {'yes' if f.value else 'no'}"
    return f"{f.claim_type}: {f.value}{(' ' + f.unit) if f.unit else ''}"
