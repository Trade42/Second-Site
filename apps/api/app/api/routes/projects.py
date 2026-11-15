from __future__ import annotations

from uuid import UUID, uuid4

from fastapi import APIRouter, Depends, Response
from sqlalchemy import select
from sqlalchemy.orm import Session

from app.agents.supervisor import run_chat
from app.agents.tools import ToolContext
from app.agents.voice_session import voice_session
from app.api.deps import container, current_user, db_session, owned_project
from app.contracts import (
    ActorType,
    BusinessExpansionProfile,
    ChatRequest,
    ChatResponse,
    ConfirmationStatus,
    CreateProjectRequest,
    EventType,
    ProjectContext,
    ProjectEnvelope,
    VoiceSessionRequest,
    VoiceSessionResponse,
)
from app.core.container import Container
from app.db import models as m
from app.repositories import registry as repos

router = APIRouter(prefix="/api/v1/projects", tags=["projects"])


def _ensure_user(db: Session, user_id: UUID) -> None:
    if db.get(m.User, user_id) is None:
        db.add(m.User(id=user_id, display_name="Owner"))
        db.flush()


@router.post("", response_model=ProjectEnvelope, status_code=201)
def create_project(
    body: CreateProjectRequest,
    user: UUID = Depends(current_user),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> ProjectEnvelope:
    _ensure_user(db, user)
    project = ProjectContext(owner_id=user, project_name=body.project_name, market=body.market)
    profile = None
    if body.business_profile:
        profile = BusinessExpansionProfile.model_validate(
            {**body.business_profile, "project_id": project.project_id}
        )
        project = project.model_copy(update={"business_profile_id": profile.profile_id})
    repos.projects.add(db, project)
    if profile:
        repos.profiles.add(db, profile)
        if profile.confirmation_status == ConfirmationStatus.CONFIRMED:
            c.events.emit(
                db,
                event_type=EventType.PROJECT_PROFILE_CONFIRMED,
                project_id=project.project_id,
                actor_type=ActorType.USER,
                actor_id=str(user),
                idempotency_key=f"profile:{profile.profile_id}:confirmed",
                payload={"profile_id": str(profile.profile_id)},
            )
    return ProjectEnvelope(project=project, business_profile=profile)


@router.get("", response_model=list[ProjectContext])
def list_projects(
    user: UUID = Depends(current_user), db: Session = Depends(db_session)
) -> list[ProjectContext]:
    return [
        ProjectContext.model_validate(r.data)
        for r in db.scalars(
            select(m.Project).where(m.Project.owner_id == user).order_by(m.Project.created_at)
        )
    ]


@router.get("/{project_id}", response_model=ProjectEnvelope)
def get_project(
    project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)
) -> ProjectEnvelope:
    profiles = repos.profiles.list(db, project_id=project.project_id)
    return ProjectEnvelope(project=project, business_profile=profiles[-1] if profiles else None)


@router.put("/{project_id}/business-profile", response_model=BusinessExpansionProfile)
def save_profile(
    body: dict,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    user: UUID = Depends(current_user),
    c: Container = Depends(container),
) -> BusinessExpansionProfile:
    profile = BusinessExpansionProfile.model_validate(
        {**body, "project_id": project.project_id, "profile_id": body.get("profile_id") or uuid4()}
    )
    repos.profiles.upsert(db, profile)
    repos.projects.upsert(db, project.model_copy(update={"business_profile_id": profile.profile_id}))
    if profile.confirmation_status == ConfirmationStatus.CONFIRMED:
        c.events.emit(
            db,
            event_type=EventType.PROJECT_PROFILE_CONFIRMED,
            project_id=project.project_id,
            actor_type=ActorType.USER,
            actor_id=str(user),
            idempotency_key=f"profile:{profile.profile_id}:confirmed:{hash(profile.model_dump_json())}",
            payload={"profile_id": str(profile.profile_id)},
        )
    return profile


@router.post("/{project_id}/chat", response_model=ChatResponse)
def chat(
    body: ChatRequest,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> ChatResponse:
    ctx = ToolContext(db=db, c=c, project_id=project.project_id, owner_id=project.owner_id)
    return run_chat(ctx, body.message, c.settings)


@router.post("/{project_id}/voice/session", response_model=VoiceSessionResponse)
def voice_session_route(
    body: VoiceSessionRequest,
    response: Response,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> VoiceSessionResponse:
    """A short-lived xAI voice session grounded like the chat (app/agents/voice_session.py). Never cached."""
    response.headers["Cache-Control"] = "no-store"
    ctx = ToolContext(db=db, c=c, project_id=project.project_id, owner_id=project.owner_id)
    return voice_session(ctx)
