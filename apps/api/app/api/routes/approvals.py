from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends
from sqlalchemy.orm import Session

from app.api.deps import container, current_user, db_session, owned_project
from app.contracts import ApprovalDecision, ApprovalRequest, ProjectContext
from app.core.container import Container
from app.repositories import registry as repos

router = APIRouter(tags=["approvals"])


@router.post("/api/v1/approvals/{approval_id}/decision", response_model=ApprovalRequest)
def decide(
    approval_id: UUID,
    body: ApprovalDecision,
    user: UUID = Depends(current_user),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> ApprovalRequest:
    return c.approvals.decide(db, approval_id, user, body.decision, body.action_payload_hash)


@router.get("/api/v1/projects/{project_id}/approvals", response_model=list[ApprovalRequest])
def list_approvals(project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)):
    return repos.approvals.list(db, project_id=project.project_id)
