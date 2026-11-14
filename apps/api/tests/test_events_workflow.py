"""Cross-domain event chain: new permit evidence -> assessment -> cost -> ranking -> activity."""

from __future__ import annotations

from uuid import uuid4

from sqlalchemy import select

from app.contracts import (
    ActorType,
    DataOrigin,
    EventType,
    EvidenceClassification,
    PropertySearchRequest,
    SourceEvidence,
)
from app.db import models as m
from app.repositories import registry as repos
from app.workflows.property_analysis import PropertyAnalysisWorkflow

from .helpers import demo_profile


def _seed(db, container):
    owner = uuid4()
    db.add(m.User(id=owner, display_name="o"))
    db.flush()
    from app.contracts import ProjectContext

    project = ProjectContext(owner_id=owner, project_name="t", market={"city": "Los Angeles", "state": "CA"})
    repos.projects.add(db, project)
    profile = demo_profile(opening_capital=None).model_copy(update={"project_id": project.project_id})
    repos.profiles.add(db, profile)
    PropertyAnalysisWorkflow(container).run(db, project.project_id, PropertySearchRequest(), uuid4())
    return project


def test_evidence_update_recomputes_downstream(db, container):
    project = _seed(db, container)
    props = {
        p.property_identity.street_address: p
        for p in repos.properties.list(db, project_id=project.project_id)
    }
    hw = props["6922 W HOLLYWOOD BLVD"]
    before = {r.ranking_version for r in repos.rankings.list(db, project_id=project.project_id)}
    assert before == {1}
    ev = SourceEvidence(
        project_id=project.project_id,
        property_id=hw.property_id,
        provider="owner",
        data_origin=DataOrigin.USER_PROVIDED,
        observation_type="electrical_service_amps",
        extracted_value=200,
        unit="A",
        evidence_classification=EvidenceClassification.OWNER_REPORTED,
        limitations=["Reported by property manager on a call; not inspected."],
    )
    container.evidence.add(db, ev)
    cause = container.events.emit(
        db,
        event_type=EventType.INFRASTRUCTURE_EVIDENCE_UPDATED,
        project_id=project.project_id,
        actor_type=ActorType.AGENT,
        actor_id="voice",
        idempotency_key=f"evidence:{ev.evidence_id}",
        payload={
            "property_id": str(hw.property_id),
            "evidence_ids": [str(ev.evidence_id)],
            "reason": "property manager answered electrical question",
        },
    )
    assert cause is not None
    rows = db.scalars(
        select(m.ActivityEventRow)
        .where(m.ActivityEventRow.correlation_id == cause.correlation_id)
        .order_by(m.ActivityEventRow.seq)
    ).all()
    chain = [r.event_type for r in rows]
    assert chain[0] == "infrastructure.evidence_updated"
    assert "property.analysis.completed" in chain and "financial.scenario.updated" in chain
    assert chain[-1] == "property.ranking.updated"
    ranking_row = rows[-1]
    scenario_row = next(r for r in rows if r.event_type == "financial.scenario.updated")
    assert ranking_row.causation_id == scenario_row.id  # causation tracked hop by hop
    assert {r.ranking_version for r in repos.rankings.list(db, project_id=project.project_id)} == {1, 2}
    # Old outputs are superseded, not deleted.
    all_scen = db.scalars(
        select(m.CostScenarioRow).where(m.CostScenarioRow.property_id == hw.property_id)
    ).all()
    assert sum(1 for s in all_scen if not s.superseded) == 1 and len(all_scen) == 2

    # Idempotency: re-emitting the same logical event is a no-op; redelivery doesn't re-run handlers.
    assert (
        container.events.emit(
            db,
            event_type=EventType.INFRASTRUCTURE_EVIDENCE_UPDATED,
            project_id=project.project_id,
            actor_type=ActorType.AGENT,
            actor_id="voice",
            idempotency_key=f"evidence:{ev.evidence_id}",
            payload=cause.payload,
        )
        is None
    )
    container.events.dispatch(db, cause)
    assert {r.ranking_version for r in repos.rankings.list(db, project_id=project.project_id)} == {1, 2}


def test_conflicting_evidence_coexists(db, container):
    project = _seed(db, container)
    alh = next(
        p
        for p in repos.properties.list(db, project_id=project.project_id)
        if p.property_identity.street_address.startswith("5380")
    )
    container.evidence.add(
        db,
        SourceEvidence(
            project_id=project.project_id,
            property_id=alh.property_id,
            provider="owner",
            data_origin=DataOrigin.USER_PROVIDED,
            observation_type="electrical_service_amps",
            extracted_value=400,
            unit="A",
            evidence_classification=EvidenceClassification.OWNER_REPORTED,
            limitations=["Broker statement"],
        ),
    )
    ev = container.evidence.for_property(db, alh.property_id)
    conflicts = container.evidence.conflicts(ev)
    assert "electrical_service_amps" in conflicts
    assert {str(e.extracted_value) for e in conflicts["electrical_service_amps"]} >= {"800.0", "400"}
