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


def _request_transaction() -> Iterator[Session]:
    yield from get_db()


def db_session(db: Session = Depends(_request_transaction, scope="function")) -> Session:
    """The request's one session. `scope="function"` makes FastAPI finish `get_db` (commit, or rollback on an
    exception, then close) as soon as the path function has returned and its response has been built, and before the
    response is sent. So a 2xx means the transaction has committed, and a refused commit becomes an error answer.
    (The default "request" scope finishes yield dependencies only after the response has been sent.) This function is
    a plain function so that every existing `Depends(db_session)`, direct or through `owned_project`, keeps sharing
    this one session without changing any route. Nothing may use the session after the path function returns: the
    events stream opens its own sessions."""
    return db


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
