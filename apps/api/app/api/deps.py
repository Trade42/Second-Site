from __future__ import annotations

from collections.abc import Iterator
from uuid import UUID

from fastapi import Depends, Header
from sqlalchemy.orm import Session

from app.contracts import ProjectContext
from app.core.container import Container, get_container
from app.core.errors import UnauthorizedError
from app.core.security import verify_token
from app.db.session import get_db


def db_session() -> Iterator[Session]:
    yield from get_db()


def container() -> Container:
    return get_container()


def current_user(authorization: str | None = Header(default=None), c: Container = Depends(container)) -> UUID:
    if not authorization or not authorization.lower().startswith("bearer "):
        raise UnauthorizedError("missing bearer token")
    return verify_token(c.settings.auth_secret.get_secret_value(), authorization.split(" ", 1)[1].strip())


def owned_project(
    project_id: UUID,
    user: UUID = Depends(current_user),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> ProjectContext:
    return c.authz.project_for_user(db, project_id, user)
