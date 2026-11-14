"""AuthorizationService: project-scoped access + tool permission categories."""

from __future__ import annotations

from enum import StrEnum
from uuid import UUID

from sqlalchemy.orm import Session

from app.contracts import ProjectContext
from app.core.errors import NotFoundError
from app.repositories import registry as repos


class ToolCategory(StrEnum):
    READ_ONLY = "READ_ONLY"
    INTERNAL_WRITE = "INTERNAL_WRITE"
    EXTERNAL_COMMUNICATION = "EXTERNAL_COMMUNICATION"
    CONSEQUENTIAL = "CONSEQUENTIAL"


APPROVAL_REQUIRED = {ToolCategory.EXTERNAL_COMMUNICATION, ToolCategory.CONSEQUENTIAL}


class AuthorizationService:
    def project_for_user(self, db: Session, project_id: UUID, user_id: UUID) -> ProjectContext:
        project = repos.projects.get(db, project_id)
        # 404 (not 403) so other owners' project IDs are not discoverable.
        if project is None or project.owner_id != user_id:
            raise NotFoundError("project not found")
        return project
