"""Seed the demo owner, phone, and facial-studio project once. Idempotent."""

from __future__ import annotations

import json
import os
from pathlib import Path
from uuid import uuid4

from sqlalchemy import select

from app.contracts import (
    ActorType,
    BusinessExpansionProfile,
    ConfirmationStatus,
    EventType,
    ProjectContext,
)
from app.core.container import get_container
from app.db import models as m
from app.db.session import session_factory
from app.domains.communications.owner_channel import channel_for_owner, register_channel
from app.repositories import registry as repos

EMAIL = "demo-owner@second-site.invalid"
FIXTURE = Path("/repo/fixtures/projects/facial_studio.json")


def main() -> None:
    db = session_factory()()
    c = get_container()
    try:
        user = db.scalar(select(m.User).where(m.User.email == EMAIL))
        if user is None:
            user = m.User(id=uuid4(), display_name="Demo Owner", email=EMAIL)
            db.add(user)
            db.flush()
        handle = os.environ.get("PHOTON_OWNER_HANDLE", "").strip()
        if handle and channel_for_owner(db, user.id) is None:
            register_channel(db, user.id, handle, "imessage")
        existing = repos.projects.list(db, owner_id=user.id)
        if not existing and FIXTURE.is_file():
            raw = json.loads(FIXTURE.read_text())
            project = ProjectContext(
                owner_id=user.id, project_name=raw["project_name"], market=raw["market"]
            )
            profile = BusinessExpansionProfile.model_validate(
                {**raw["business_profile"], "project_id": project.project_id}
            )
            project = project.model_copy(update={"business_profile_id": profile.profile_id})
            repos.projects.add(db, project)
            repos.profiles.add(db, profile)
            if profile.confirmation_status == ConfirmationStatus.CONFIRMED:
                c.events.emit(
                    db,
                    event_type=EventType.PROJECT_PROFILE_CONFIRMED,
                    project_id=project.project_id,
                    actor_type=ActorType.USER,
                    actor_id=str(user.id),
                    idempotency_key=f"profile:{profile.profile_id}:confirmed",
                    payload={"profile_id": str(profile.profile_id)},
                )
        db.commit()
        print("seed ok")
    finally:
        db.close()


if __name__ == "__main__":
    main()
