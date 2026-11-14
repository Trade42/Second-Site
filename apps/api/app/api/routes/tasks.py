from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends
from sqlalchemy.orm import Session

from app.api.deps import db_session, owned_project
from app.contracts import ProjectContext, TaskRecord
from app.core.errors import NotFoundError
from app.repositories import registry as repos

router = APIRouter(prefix="/api/v1/projects/{project_id}", tags=["tasks"])


@router.get("/tasks/{task_id}", response_model=TaskRecord)
def get_task(
    task_id: UUID, project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)
):
    t = repos.tasks.get(db, task_id)
    if t is None or t.project_id != project.project_id:
        raise NotFoundError("task not found")
    return t
