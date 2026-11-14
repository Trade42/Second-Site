from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends
from sqlalchemy.orm import Session

from app.api.deps import container, current_user, db_session, owned_project
from app.contracts import (
    ActionType,
    ActorType,
    ApprovalRequest,
    CallSession,
    Channel,
    CommunicationSession,
    CommunicationState,
    Contract,
    EventType,
    HandoffStatus,
    OutreachRequest,
    ProjectContext,
)
from app.core.container import Container
from app.repositories import registry as repos
from app.services.call_orchestration import CallOrchestrator, call_payload

router = APIRouter(prefix="/api/v1/projects/{project_id}", tags=["communications"])


class OutreachAccepted(Contract):
    communication: CommunicationSession
    approval: ApprovalRequest


class StartCallBody(Contract):
    communication_id: UUID


class HandoffResponseBody(Contract):
    response: HandoffStatus


@router.post("/outreach", response_model=OutreachAccepted, status_code=202)
def outreach(
    body: OutreachRequest,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> OutreachAccepted:
    """Creates a communication draft + PENDING approval. Nothing is sent until the owner approves
    and the communications workstream executes it via /calls."""
    payload = call_payload(project.project_id, body.recipient, body.property_id)
    appr = c.approvals.request(
        db,
        project_id=project.project_id,
        owner_id=project.owner_id,
        agent="outreach",
        action_type=ActionType.PLACE_CALL if body.channel == Channel.VOICE else ActionType.SEND_MESSAGE,
        payload=payload,
        summary=f"{body.channel.value} to {body.recipient.get('name', 'contact')}: {body.purpose}",
        idempotency_key=f"outreach:{project.project_id}:{body.property_id}:{body.recipient}",
    )
    comm = CommunicationSession(
        project_id=project.project_id,
        property_id=body.property_id,
        channel=body.channel,
        external_participants=[body.recipient],
        authorized_owner_id=project.owner_id,
        purpose=body.purpose,
        state=CommunicationState.AWAITING_APPROVAL,
        approval_id=appr.approval_id,
        recording_reference=body.intro_recording_reference,
    )
    repos.communications.add(db, comm)
    c.events.emit(
        db,
        event_type=EventType.OUTREACH_APPROVAL_REQUESTED,
        project_id=project.project_id,
        actor_type=ActorType.AGENT,
        actor_id="outreach",
        idempotency_key=f"outreach:{comm.communication_id}:requested",
        payload={
            "communication_id": str(comm.communication_id),
            "property_id": str(body.property_id),
            "approval_id": str(appr.approval_id),
        },
    )
    return OutreachAccepted(communication=comm, approval=appr)


@router.get("/communications", response_model=list[CommunicationSession])
def list_comms(project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)):
    return repos.communications.list(db, project_id=project.project_id)


@router.post("/calls", response_model=CallSession, status_code=202)
def start_call(
    body: StartCallBody,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> CallSession:
    comm = repos.communications.require(db, body.communication_id)
    recipient = comm.external_participants[0] if comm.external_participants else {}
    return CallOrchestrator(c).start(
        db,
        project_id=project.project_id,
        owner_id=project.owner_id,
        recipient=recipient,
        property_id=comm.property_id,
        approval_id=comm.approval_id,
        intro_recording_reference=comm.recording_reference,
    )


@router.get("/calls", response_model=list[CallSession])
def list_calls(project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)):
    return repos.calls.list(db, project_id=project.project_id)


@router.post("/handoffs/{handoff_id}/response", response_model=CallSession)
def handoff_response(
    handoff_id: UUID,
    body: HandoffResponseBody,
    project: ProjectContext = Depends(owned_project),
    user: UUID = Depends(current_user),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> CallSession:
    return CallOrchestrator(c).owner_response(db, handoff_id, user, body.response)
