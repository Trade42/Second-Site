"""Property analysis vertical slice + event-driven recomputation.

INPUT → VALIDATION → PROCESSING → PERSISTED OUTPUT → EVENT → NEXT CONSUMER

search → identity → permits → digital profile → feasibility → cost scenario → ranking.
Subscribers:
  infrastructure.evidence_updated  → re-evaluate that property  → financial.scenario.updated
  financial.scenario.updated       → re-rank project            → property.ranking.updated
"""

from __future__ import annotations

from datetime import UTC, datetime
from uuid import UUID

from sqlalchemy import func, select, update
from sqlalchemy.orm import Session

from app.contracts import (
    ActivityEvent,
    ActorType,
    AssessmentStatus,
    BusinessExpansionProfile,
    CostScenario,
    EventType,
    InfrastructureAssessment,
    ProjectStatus,
    PropertyDigitalProfile,
    PropertyRankingResult,
    PropertySearchRequest,
)
from app.core.container import Container, get_container
from app.core.errors import ConflictError, NotFoundError
from app.db import models as m
from app.domains.feasibility.engine import FeasibilityEngine
from app.domains.finance.calculator import FinancialCalculationService
from app.domains.permits.service import PermitIntelligenceService
from app.domains.property.service import PropertyScoutService, attach_permits
from app.domains.ranking.engine import RankingEngine
from app.domains.requirements.templates import requirements_for
from app.repositories import registry as repos
from app.services.events import subscribe

AGENT = "expansion-supervisor"


def current_profile(db: Session, project_id: UUID) -> BusinessExpansionProfile:
    profiles = repos.profiles.list(db, project_id=project_id)
    if not profiles:
        raise NotFoundError("business profile not found; create one first", error_code="PROFILE_REQUIRED")
    return profiles[-1]


class PropertyAnalysisWorkflow:
    def __init__(self, c: Container | None = None):
        self.c = c or get_container()
        self.scout = PropertyScoutService(self.c.costar)
        self.permit_svc = PermitIntelligenceService(self.c.permits)
        self.feasibility = FeasibilityEngine()
        self.finance = FinancialCalculationService()
        self.ranking = RankingEngine()

    # -- steps ---------------------------------------------------------------
    def search(
        self, db: Session, project_id: UUID, req: PropertySearchRequest, correlation_id: UUID
    ) -> list[PropertyDigitalProfile]:
        found = self.scout.search(project_id, req)
        saved: list[PropertyDigitalProfile] = []
        for p in found:
            existing = db.scalar(
                select(m.Property).where(
                    m.Property.project_id == project_id,
                    m.Property.normalized_address == p.property_identity.normalized_address,
                )
            )
            if existing is not None:
                saved.append(PropertyDigitalProfile.model_validate(existing.data))
                continue
            repos.properties.add(db, p)
            saved.append(p)
            self.c.events.emit(
                db,
                event_type=EventType.PROPERTY_IDENTITY_RESOLVED,
                project_id=project_id,
                actor_type=ActorType.AGENT,
                actor_id="property-scout",
                correlation_id=correlation_id,
                idempotency_key=f"identity:{p.property_id}",
                payload={
                    "property_id": str(p.property_id),
                    "summary": f"{p.property_identity.normalized_address} "
                    f"({p.property_identity.identity_match_status.value})",
                },
            )
        origin = (
            saved[0].commercial_listing.data_origin.value if saved and saved[0].commercial_listing else "NONE"
        )
        self.c.events.emit(
            db,
            event_type=EventType.PROPERTY_SEARCH_COMPLETED,
            project_id=project_id,
            actor_type=ActorType.AGENT,
            actor_id="property-scout",
            correlation_id=correlation_id,
            idempotency_key=f"search:{project_id}:{correlation_id}",
            payload={
                "property_ids": [str(p.property_id) for p in saved],
                "provider": getattr(self.c.costar, "mode", "fixture"),
                "data_origin": origin,
            },
        )
        return saved

    def ingest_permits(
        self, db: Session, prop: PropertyDigitalProfile, correlation_id: UUID
    ) -> PropertyDigitalProfile:
        res = self.permit_svc.ingest(db, prop.property_identity, prop.project_id)
        all_permits = repos.permits.list(db, property_id=prop.property_id)
        origin = "LIVE" if getattr(self.c.permits, "mode", "") == "live" else "FIXTURE"
        prop = attach_permits(prop, all_permits, res.datasets, origin)
        repos.properties.upsert(db, prop)
        self.c.events.emit(
            db,
            event_type=EventType.PROPERTY_PERMITS_INGESTED,
            project_id=prop.project_id,
            actor_type=ActorType.AGENT,
            actor_id="permit-intelligence",
            correlation_id=correlation_id,
            idempotency_key=f"permits:{prop.property_id}:{len(all_permits)}",
            payload={
                "property_id": str(prop.property_id),
                "permit_record_ids": [str(p.permit_record_id) for p in res.permits],
                "evidence_ids": [str(e.evidence_id) for e in res.evidence],
                "source_dataset_ids": res.datasets,
            },
        )
        return prop

    def evaluate(
        self,
        db: Session,
        prop: PropertyDigitalProfile,
        correlation_id: UUID,
        cause: ActivityEvent | None = None,
    ) -> tuple[list[InfrastructureAssessment], CostScenario]:
        profile = current_profile(db, prop.project_id)
        evidence = self.c.evidence.for_property(db, prop.property_id)
        assessments, work = self.feasibility.evaluate(profile, requirements_for(profile), prop, evidence)
        # Supersede previous outputs instead of deleting them (history preserved).
        for model in (m.InfrastructureAssessmentRow, m.RenovationWorkItemRow, m.CostScenarioRow):
            db.execute(update(model).where(model.property_id == prop.property_id).values(superseded=True))
        repos.assessments.add_all(db, assessments)
        repos.work_items.add_all(db, work)
        scenario = self.finance.scenario(profile, prop, work)
        repos.cost_scenarios.add(db, scenario)
        prop = prop.model_copy(
            update={"infrastructure_assessments": assessments, "last_analyzed_at": datetime.now(UTC)}
        )
        repos.properties.upsert(db, prop)
        tag = cause.event_id if cause else correlation_id
        unknown = [a for a in assessments if a.status == AssessmentStatus.UNKNOWN]
        self.c.events.emit(
            db,
            event_type=EventType.PROPERTY_ANALYSIS_COMPLETED,
            project_id=prop.project_id,
            actor_type=ActorType.AGENT,
            actor_id="feasibility",
            correlation_id=correlation_id,
            causation=cause,
            idempotency_key=f"analysis:{prop.property_id}:{tag}",
            payload={
                "property_id": str(prop.property_id),
                "assessment_ids": [str(a.assessment_id) for a in assessments],
                "unknown_requirement_ids": [a.requirement_id for a in unknown],
            },
        )
        if unknown:
            self.c.events.emit(
                db,
                event_type=EventType.INFRASTRUCTURE_UNKNOWN_DETECTED,
                project_id=prop.project_id,
                actor_type=ActorType.AGENT,
                actor_id="feasibility",
                correlation_id=correlation_id,
                causation=cause,
                idempotency_key=f"unknowns:{prop.property_id}:{tag}",
                payload={
                    "property_id": str(prop.property_id),
                    "requirement_ids": [a.requirement_id for a in unknown],
                    "verification_questions": [q for a in unknown for q in a.verification_questions],
                },
            )
        self.c.events.emit(
            db,
            event_type=EventType.FINANCIAL_SCENARIO_UPDATED,
            project_id=prop.project_id,
            actor_type=ActorType.AGENT,
            actor_id="financial-modeling",
            correlation_id=correlation_id,
            causation=cause,
            idempotency_key=f"scenario:{scenario.scenario_id}",
            payload={
                "property_id": str(prop.property_id),
                "scenario_id": str(scenario.scenario_id),
                "long_term_cost": scenario.long_term_cost,
                "comparison_horizon_months": scenario.comparison_horizon_months,
            },
            dispatch=cause is not None,
        )
        return assessments, scenario

    def rank(
        self, db: Session, project_id: UUID, correlation_id: UUID, cause: ActivityEvent | None = None
    ) -> PropertyRankingResult:
        profile = current_profile(db, project_id)
        props = repos.properties.list(db, project_id=project_id)
        scenarios, assessments = {}, {}
        for p in props:
            rows = db.scalars(
                select(m.CostScenarioRow).where(
                    m.CostScenarioRow.property_id == p.property_id, m.CostScenarioRow.superseded.is_(False)
                )
            ).all()
            if rows:
                scenarios[p.property_id] = CostScenario.model_validate(rows[-1].data)
            arows = db.scalars(
                select(m.InfrastructureAssessmentRow).where(
                    m.InfrastructureAssessmentRow.property_id == p.property_id,
                    m.InfrastructureAssessmentRow.superseded.is_(False),
                )
            ).all()
            assessments[p.property_id] = [InfrastructureAssessment.model_validate(r.data) for r in arows]
        version = (
            db.scalar(
                select(func.max(m.PropertyRanking.ranking_version)).where(
                    m.PropertyRanking.project_id == project_id
                )
            )
            or 0
        ) + 1
        result = self.ranking.rank(profile, props, scenarios, assessments, version)
        repos.rankings.add(db, result)
        top = result.ranked_properties[0].property_id if result.ranked_properties else None
        self.c.events.emit(
            db,
            event_type=EventType.PROPERTY_RANKING_UPDATED,
            project_id=project_id,
            actor_type=ActorType.AGENT,
            actor_id="property-ranking",
            correlation_id=correlation_id,
            causation=cause,
            idempotency_key=f"ranking:{project_id}:{version}",
            payload={
                "ranking_id": str(result.ranking_id),
                "ranking_version": version,
                "top_property_id": str(top) if top else None,
            },
        )
        return result

    # -- full slice ----------------------------------------------------------
    def run(self, db: Session, project_id: UUID, req: PropertySearchRequest, correlation_id: UUID) -> dict:
        project = repos.projects.require(db, project_id)
        props = self.search(db, project_id, req, correlation_id)
        if not props:
            raise ConflictError("no candidate properties found", error_code="NO_CANDIDATES")
        for p in props:
            p = self.ingest_permits(db, p, correlation_id)
            self.evaluate(db, p, correlation_id)
        ranking = self.rank(db, project_id, correlation_id)
        repos.projects.upsert(
            db,
            project.model_copy(
                update={"project_status": ProjectStatus.COMPARING, "updated_at": datetime.now(UTC)}
            ),
        )
        return {
            "property_ids": [str(p.property_id) for p in props],
            "ranking_id": str(ranking.ranking_id),
            "ranking_version": ranking.ranking_version,
        }


# ---- event subscribers (cross-domain consumers) ----------------------------


def already_handled(db: Session, cause: ActivityEvent, result_type: EventType) -> bool:
    """Redelivery guard: a handler's result event carries causation_id = cause.event_id."""
    return (
        db.scalar(
            select(m.ActivityEventRow.id).where(
                m.ActivityEventRow.causation_id == cause.event_id,
                m.ActivityEventRow.event_type == result_type.value,
            )
        )
        is not None
    )


@subscribe(EventType.INFRASTRUCTURE_EVIDENCE_UPDATED, "feasibility.reevaluate")
def on_evidence_updated(db: Session, event: ActivityEvent) -> None:
    if already_handled(db, event, EventType.PROPERTY_ANALYSIS_COMPLETED):
        return
    pid = UUID(event.payload["property_id"])
    prop = repos.properties.require(db, pid)
    PropertyAnalysisWorkflow().evaluate(db, prop, event.correlation_id, cause=event)


@subscribe(EventType.FINANCIAL_SCENARIO_UPDATED, "ranking.rerank")
def on_scenario_updated(db: Session, event: ActivityEvent) -> None:
    if already_handled(db, event, EventType.PROPERTY_RANKING_UPDATED):
        return
    PropertyAnalysisWorkflow().rank(db, event.project_id, event.correlation_id, cause=event)
