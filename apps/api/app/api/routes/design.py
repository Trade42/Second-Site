from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends
from pydantic import Field
from sqlalchemy.orm import Session

from app.api.deps import container, current_user, db_session, owned_project
from app.contracts import (
    ActorType,
    BillOfMaterials,
    Contract,
    DesignRevisionRequest,
    DesignVersion,
    EventType,
    LayoutGenerationRequest,
    ProjectContext,
    RoomRequirement,
    SiteGeometry,
    Subsystem,
    TaskAccepted,
    TaskType,
)
from app.core.container import Container
from app.core.errors import NotFoundError
from app.domains.design.service import DesignService
from app.domains.procurement.bom import generate_bom
from app.repositories import registry as repos

router = APIRouter(prefix="/api/v1/projects/{project_id}/layouts", tags=["design"])


class RevisionBody(Contract):
    requested_changes: list[str] = Field(min_length=1)
    room_requirement_overrides: list[RoomRequirement] = Field(default_factory=list)
    preserved_constraints: list[str] = Field(default_factory=list)
    source_channel: str = "web"


def _design(db: Session, project: ProjectContext, layout_id: UUID) -> DesignVersion:
    d = repos.designs.get(db, layout_id)
    if d is None or d.project_id != project.project_id:
        raise NotFoundError("layout not found")
    return d


@router.post("/generate", response_model=TaskAccepted, status_code=202)
def generate(
    body: LayoutGenerationRequest,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> TaskAccepted:
    if repos.properties.get(db, body.property_id) is None:
        raise NotFoundError("property not found")
    svc = DesignService(c)
    task = c.tasks.create(
        db,
        project.project_id,
        TaskType.GENERATE_LAYOUT,
        Subsystem.PROPERTY_SPATIAL,
        body.model_dump(mode="json"),
    )
    task = c.tasks.run(
        db,
        task,
        lambda t: {
            "design_version_ids": [
                str(d.design_version_id) for d in svc.generate(db, project.project_id, body, t.correlation_id)
            ]
        },
    )
    return TaskAccepted(task=task, status_url=f"/api/v1/projects/{project.project_id}/tasks/{task.task_id}")


@router.get("/geometry/{geometry_id}", response_model=SiteGeometry)
def get_geometry(
    geometry_id: UUID, project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)
):
    """The measured (or fixture) floor a layout was generated on: outline, partitions, openings."""
    geo = repos.geometries.get(db, geometry_id)
    prop = repos.properties.get(db, geo.property_id) if geo else None
    if geo is None or prop is None or prop.project_id != project.project_id:
        raise NotFoundError("geometry not found")
    return geo


@router.get("", response_model=list[DesignVersion])
def list_layouts(project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)):
    return repos.designs.list(db, project_id=project.project_id)


@router.post("/{layout_id}/revisions", response_model=TaskAccepted, status_code=202)
def revise(
    layout_id: UUID,
    body: RevisionBody,
    project: ProjectContext = Depends(owned_project),
    user: UUID = Depends(current_user),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> TaskAccepted:
    base = _design(db, project, layout_id)
    rev = DesignRevisionRequest(
        project_id=project.project_id,
        property_id=base.property_id,
        base_design_version_id=base.design_version_id,
        requested_by=user,
        **body.model_dump(),
    )
    svc = DesignService(c)
    task = c.tasks.create(
        db,
        project.project_id,
        TaskType.REVISE_LAYOUT,
        Subsystem.PROPERTY_SPATIAL,
        {"revision_request_id": str(rev.revision_request_id)},
    )

    def work(t):
        updated, created = svc.revise(db, rev, t.correlation_id)
        return {
            "revision_status": updated.status.value,
            "design_version_ids": [str(d.design_version_id) for d in created],
            "current_valid_version_id": str(
                created[0].design_version_id if created else base.design_version_id
            ),
        }

    task = c.tasks.run(db, task, work)
    return TaskAccepted(task=task, status_url=f"/api/v1/projects/{project.project_id}/tasks/{task.task_id}")


@router.get("/{layout_id}/bom", response_model=BillOfMaterials)
def bom(
    layout_id: UUID,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> BillOfMaterials:
    d = _design(db, project, layout_id)
    existing = repos.boms.list(db, design_version_id=d.design_version_id)
    if existing:
        return existing[-1]
    b = generate_bom(d)
    repos.boms.add(db, b)
    c.events.emit(
        db,
        event_type=EventType.BOM_GENERATED,
        project_id=project.project_id,
        actor_type=ActorType.AGENT,
        actor_id="procurement",
        idempotency_key=f"bom:{b.bom_id}",
        payload={
            "bom_id": str(b.bom_id),
            "design_version_id": str(d.design_version_id),
            "estimated_total": b.estimated_total,
        },
    )
    return b
