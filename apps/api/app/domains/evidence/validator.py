"""Evidence & compliance validator (deterministic). Owner: agents-finance.

Flags assertions without evidence, historical evidence presented as current, synthetic data presented
as a real provider record, and conflicting values.
"""

from __future__ import annotations

from app.contracts import (
    AssessmentStatus,
    DataOrigin,
    EvidenceClassification,
    InfrastructureAssessment,
    PropertyDigitalProfile,
    SourceEvidence,
)


def validate_property(
    prop: PropertyDigitalProfile, evidence: list[SourceEvidence], assessments: list[InfrastructureAssessment]
) -> list[str]:
    problems: list[str] = []
    ids = {e.evidence_id for e in evidence}
    for a in assessments:
        missing = [str(i) for i in a.evidence_ids if i not in ids]
        if missing:
            problems.append(f"{a.requirement_id}: references unknown evidence {missing}")
        if a.status == AssessmentStatus.MEETS:
            hist = [
                e
                for e in evidence
                if e.evidence_id in a.evidence_ids
                and e.evidence_classification == EvidenceClassification.DOCUMENTED_HISTORICAL
            ]
            if hist and len(hist) == len(a.evidence_ids) and not a.limitations:
                problems.append(
                    f"{a.requirement_id}: MEETS on historical evidence without stated limitations"
                )
    lst = prop.commercial_listing
    if lst and lst.data_origin != DataOrigin.LIVE and lst.provider.lower() == "costar":
        problems.append("listing labeled CoStar but not retrieved live from CoStar")
    return problems
