from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends
from sqlalchemy.orm import Session

from app.api.deps import container, db_session, owned_project
from app.contracts import (
    CostScenario,
    InfrastructureAssessment,
    PermitRecord,
    ProjectContext,
    PropertyDigitalProfile,
    PropertyRankingResult,
    PropertySearchRequest,
    Subsystem,
    TaskAccepted,
    TaskType,
)
from app.core.container import Container
from app.core.errors import NotFoundError
from app.db import models as m
from app.repositories import registry as repos
from app.workflows.property_analysis import PropertyAnalysisWorkflow

router = APIRouter(prefix="/api/v1/projects/{project_id}", tags=["properties"])


def _prop(db: Session, project: ProjectContext, property_id: UUID) -> PropertyDigitalProfile:
    p = repos.properties.get(db, property_id)
    if p is None or p.project_id != project.project_id:
        raise NotFoundError("property not found")
    return p


def _accepted(task, project_id) -> TaskAccepted:
    return TaskAccepted(task=task, status_url=f"/api/v1/projects/{project_id}/tasks/{task.task_id}")


@router.post("/properties/search", response_model=TaskAccepted, status_code=202)
def search(
    body: PropertySearchRequest,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> TaskAccepted:
    """Runs the full property-analysis slice as a task (inline in dev; worker in redis mode)."""
    wf = PropertyAnalysisWorkflow(c)
    task = c.tasks.create(
        db,
        project.project_id,
        TaskType.ANALYZE_PROPERTY,
        Subsystem.AGENTS_FINANCE,
        body.model_dump(mode="json"),
    )
    task = c.tasks.run(db, task, lambda t: wf.run(db, project.project_id, body, t.correlation_id))
    return _accepted(task, project.project_id)


@router.get("/properties", response_model=list[PropertyDigitalProfile])
def list_properties(project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)):
    return repos.properties.list(db, project_id=project.project_id)


@router.get("/properties/{property_id}", response_model=PropertyDigitalProfile)
def get_property(
    property_id: UUID, project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)
):
    return _prop(db, project, property_id)


@router.post("/properties/{property_id}/analyze", response_model=TaskAccepted, status_code=202)
def analyze(
    property_id: UUID,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> TaskAccepted:
    prop = _prop(db, project, property_id)
    wf = PropertyAnalysisWorkflow(c)
    task = c.tasks.create(
        db,
        project.project_id,
        TaskType.EVALUATE_INFRASTRUCTURE,
        Subsystem.AGENTS_FINANCE,
        {"property_id": str(property_id)},
    )

    def work(t):
        p = wf.ingest_permits(db, prop, t.correlation_id)
        a, s = wf.evaluate(db, p, t.correlation_id)
        r = wf.rank(db, project.project_id, t.correlation_id)
        return {
            "assessment_ids": [str(x.assessment_id) for x in a],
            "scenario_id": str(s.scenario_id),
            "ranking_id": str(r.ranking_id),
        }

    return _accepted(c.tasks.run(db, task, work), project.project_id)


@router.get("/properties/{property_id}/permits", response_model=list[PermitRecord])
def permits(
    property_id: UUID, project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)
):
    _prop(db, project, property_id)
    return repos.permits.list(db, property_id=property_id)


@router.get("/properties/{property_id}/feasibility", response_model=list[InfrastructureAssessment])
def feasibility(
    property_id: UUID, project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)
):
    _prop(db, project, property_id)
    return repos.assessments.list(db, property_id=property_id, superseded=False)


@router.get("/rankings", response_model=list[PropertyRankingResult])
def rankings(project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)):
    return repos.rankings.list(db, project_id=project.project_id, order_by=m.PropertyRanking.ranking_version)


@router.get("/cost-scenarios", response_model=list[CostScenario])
def cost_scenarios(project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)):
    return repos.cost_scenarios.list(db, project_id=project.project_id, superseded=False)
