"""Development-only helpers (disabled unless ALLOW_DEV_TOKENS=true)."""

from __future__ import annotations

import json
from uuid import uuid4

from fastapi import APIRouter, Depends
from sqlalchemy import select
from sqlalchemy.orm import Session

from app.api.deps import container, db_session
from app.contracts import DevTokenResponse
from app.core.container import Container
from app.core.errors import ForbiddenError
from app.core.security import issue_token
from app.db import models as m
from app.integrations.base import FIXTURES_DIR

router = APIRouter(prefix="/api/v1/dev", tags=["dev"])
DEMO_EMAIL = "demo-owner@second-site.invalid"


@router.post("/token", response_model=DevTokenResponse)
def dev_token(db: Session = Depends(db_session), c: Container = Depends(container)) -> DevTokenResponse:
    if not c.settings.allow_dev_tokens or c.settings.app_env == "production":
        raise ForbiddenError("dev tokens disabled")
    user = db.scalar(select(m.User).where(m.User.email == DEMO_EMAIL))
    if user is None:
        user = m.User(id=uuid4(), display_name="Demo Owner", email=DEMO_EMAIL)
        db.add(user)
        db.flush()
    return DevTokenResponse(
        user_id=user.id, access_token=issue_token(c.settings.auth_secret.get_secret_value(), user.id)
    )


@router.get("/fixtures/project")
def demo_project_fixture() -> dict:
    """The facial-studio demo inputs (FIXTURE_SYNTHETIC) for the onboarding form."""
    return json.loads((FIXTURES_DIR / "projects" / "facial_studio.json").read_text())
