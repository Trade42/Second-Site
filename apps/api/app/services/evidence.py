"""EvidenceService: query sourced evidence; conflicting values coexist and are surfaced, not merged."""

from __future__ import annotations

from collections import defaultdict
from uuid import UUID

from sqlalchemy.orm import Session

from app.contracts import SourceEvidence
from app.repositories import registry as repos


class EvidenceService:
    def for_property(self, db: Session, property_id: UUID) -> list[SourceEvidence]:
        return repos.evidence.list(db, property_id=property_id)

    def add(self, db: Session, evidence: SourceEvidence) -> SourceEvidence:
        return repos.evidence.add(db, evidence)

    @staticmethod
    def conflicts(evidence: list[SourceEvidence]) -> dict[str, list[SourceEvidence]]:
        """observation_type -> evidence when sources disagree on the value."""
        groups: dict[str, list[SourceEvidence]] = defaultdict(list)
        for e in evidence:
            groups[e.observation_type].append(e)
        return {k: v for k, v in groups.items() if len({str(e.extracted_value) for e in v}) > 1}
