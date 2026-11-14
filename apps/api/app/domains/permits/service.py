"""PermitIntelligenceService: PropertyIdentity -> matched PermitRecords + SourceEvidence.

Producer for Feasibility. Stores the raw source record separately (property_sources) and only
normalized PermitRecord/SourceEvidence in consumer-facing tables.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime
from uuid import UUID, uuid4

from sqlalchemy import select
from sqlalchemy.orm import Session

from app.contracts import (
    EvidenceClassification,
    MatchStatus,
    PermitRecord,
    PropertyIdentity,
    SourceEvidence,
    VerificationStatus,
)
from app.db import models as m
from app.domains.property.identity import match_record, parsed_identity
from app.integrations.la_permits import LAPermitProvider, RawPermit
from app.repositories import registry as repos

from .extract import category_for, extract_facts

FINALED = {"PERMIT FINALED"}


@dataclass
class PermitIngestResult:
    permits: list[PermitRecord] = field(default_factory=list)
    evidence: list[SourceEvidence] = field(default_factory=list)
    rejected: list[tuple[str, str]] = field(default_factory=list)  # (permit_nbr, reason)
    datasets: list[str] = field(default_factory=list)


def _date(v: str | None):
    return datetime.fromisoformat(v).date() if v else None


def _dt(v: str | None):
    return datetime.fromisoformat(v).replace(tzinfo=UTC) if v else None


def normalize_permit(raw: RawPermit, identity: PropertyIdentity) -> tuple[PermitRecord | None, str]:
    r = raw.record
    result = match_record(identity, r.get("primary_address", ""), r.get("zip_code"), r.get("apn"))
    if not result.matched:
        return None, result.explanation
    category = category_for(r.get("permit_group"), r.get("permit_type"))
    status = r.get("status_desc", "UNKNOWN")
    from app.domains.property.address import parse_street

    pa = parse_street(
        r["primary_address"], city=identity.city, state=identity.state, postal_code=r.get("zip_code")
    )
    rec = PermitRecord(
        property_id=identity.property_id,
        permit_number=r["permit_nbr"],
        permit_category=category,
        permit_status=status,
        issued_date=_date(r.get("issue_date")),
        finaled_date=_date(r.get("status_date")) if status.upper() in FINALED else None,
        work_description=r.get("work_desc", ""),
        original_address=r["primary_address"],
        normalized_address=pa.normalized(),
        unit_or_suite=pa.unit,
        scope=result.scope,
        parcel_id=r.get("apn"),
        source_dataset=raw.dataset,
        source_url=raw.source_url,
        data_origin=raw.data_origin,
        matched_property_identity=identity.property_id,
        match_status=result.status,
        match_confidence=result.confidence,
        match_explanation=result.explanation,
        extracted_infrastructure_facts=extract_facts(category, r.get("work_desc", "")),
    )
    return rec, result.explanation


class PermitIntelligenceService:
    def __init__(self, provider: LAPermitProvider):
        self.provider = provider

    def fetch_and_match(
        self, identity: PropertyIdentity, project_id: UUID | None = None
    ) -> PermitIngestResult:
        p = parsed_identity(identity)
        raws = self.provider.search_by_address_prefix(
            p.house_number, p.direction, p.street_name, identity.postal_code
        )
        res = PermitIngestResult(datasets=sorted({r.dataset for r in raws}))
        for raw in raws:
            rec, why = normalize_permit(raw, identity)
            if rec is None:
                res.rejected.append((raw.record.get("permit_nbr", "?"), why))
                continue
            ev_ids: list[UUID] = []
            # Evidence that the permit exists as described (record-level).
            base = dict(
                project_id=project_id,
                property_id=identity.property_id,
                provider="la_open_data",
                dataset=raw.dataset,
                source_record_id=rec.permit_number,
                source_url=rec.source_url,
                data_origin=raw.data_origin,
                source_effective_at=_dt(raw.record.get("status_date")),
                match_confidence=rec.match_confidence,
                verification_status=VerificationStatus.SOURCE_CONFIRMED,
            )
            classification = (
                EvidenceClassification.DOCUMENTED_HISTORICAL
                if rec.match_status in (MatchStatus.EXACT, MatchStatus.PROBABLE)
                else EvidenceClassification.UNKNOWN
            )
            for fact in rec.extracted_infrastructure_facts:
                ev = SourceEvidence(
                    **base,
                    observation_type=fact.fact_type,
                    extracted_value=fact.value,
                    unit=fact.unit,
                    evidence_classification=classification,
                    limitations=fact.limitations
                    + [
                        f"Permit status at source: {rec.permit_status}",
                        f"Match: {rec.match_explanation}",
                        f"Scope: {rec.scope}",
                    ],
                )
                res.evidence.append(ev)
                ev_ids.append(ev.evidence_id)
            rec = rec.model_copy(update={"source_evidence_ids": ev_ids})
            res.permits.append(rec)
        return res

    def ingest(self, db: Session, identity: PropertyIdentity, project_id: UUID) -> PermitIngestResult:
        """Fetch, match, persist (idempotent per property+permit number)."""
        res = self.fetch_and_match(identity, project_id)
        existing = set(
            db.scalars(
                select(m.PermitRecordRow.permit_number).where(
                    m.PermitRecordRow.property_id == identity.property_id
                )
            )
        )
        kept_permits, kept_evidence = [], []
        ev_by_id = {e.evidence_id: e for e in res.evidence}
        for rec in res.permits:
            if rec.permit_number in existing:
                continue
            repos.permits.add(db, rec)
            for eid in rec.source_evidence_ids:
                repos.evidence.add(db, ev_by_id[eid])
                kept_evidence.append(ev_by_id[eid])
            db.add(
                m.PropertySource(
                    id=uuid4(),
                    property_id=identity.property_id,
                    provider="la_open_data",
                    dataset=rec.source_dataset,
                    source_record_id=rec.permit_number,
                    data_origin=rec.data_origin.value,
                    raw_payload={"permit_nbr": rec.permit_number, "source_url": rec.source_url},
                )
            )
            db.add(
                m.PropertyIdentityMatch(
                    id=uuid4(),
                    property_id=identity.property_id,
                    provider="la_open_data",
                    provider_record_id=rec.permit_number,
                    match_status=rec.match_status.value,
                    match_confidence=rec.match_confidence,
                    explanation=rec.match_explanation or "",
                )
            )
            kept_permits.append(rec)
        db.flush()
        return PermitIngestResult(kept_permits, kept_evidence, res.rejected, res.datasets)
