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
    OwnerChannel,
    OwnerHandoffRequest,
    ProjectContext,
)
from app.core.container import Container
from app.domains.communications.owner_channel import channel_for_owner, notify_owner, register_channel
from app.domains.communications.poller import poll_once
from app.repositories import registry as repos
from app.services.call_orchestration import CallOrchestrator, call_payload
from app.services.call_orchestration.simulator import CallSimulator

router = APIRouter(prefix="/api/v1/projects/{project_id}", tags=["communications"])
owner_router = APIRouter(prefix="/api/v1/owner", tags=["communications"])


class OwnerChannelBody(Contract):
    handle: str
    platform: str = "imessage"


class OwnerChannelView(Contract):
    channel: OwnerChannel | None
    agent_handle: str | None
    instructions: str


class PollResult(Contract):
    fetched: int
    new: int
    routed: int


class SimulateBody(Contract):
    owner_action: str = "accept"
    answers: dict[str, str] = {}


def _channel_view(ch: OwnerChannel | None, c: Container) -> OwnerChannelView:
    agent = c.settings.photon_agent_handle
    if ch is None:
        msg = "Add the phone number or iMessage email you text from."
    elif ch.status.value == "LINKED":
        msg = "Connected. The assistant texts you about calls, approvals and summaries."
    else:
        msg = f"Text 'hi' to {agent or 'your Photon agent number'} from {ch.handle} to connect (Photon lines can't text first)."
    return OwnerChannelView(channel=ch, agent_handle=agent, instructions=msg)


@owner_router.get("/channel", response_model=OwnerChannelView)
def get_channel(
    user: UUID = Depends(current_user), db: Session = Depends(db_session), c: Container = Depends(container)
):
    return _channel_view(channel_for_owner(db, user), c)


@owner_router.put("/channel", response_model=OwnerChannelView)
def put_channel(
    body: OwnerChannelBody,
    user: UUID = Depends(current_user),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
):
    return _channel_view(register_channel(db, user, body.handle, body.platform), c)


class SimulatedTextBody(Contract):
    text: str


class SimulatedTextResult(Contract):
    routed: bool
    reply: str | None


@owner_router.post("/channel/simulate-text", response_model=SimulatedTextResult)
def simulate_text(
    body: SimulatedTextBody,
    user: UUID = Depends(current_user),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
):
    """Fixture messaging only: pretend the owner texted the agent (to demo linking, JOIN/LATER, STATUS, chat)."""
    if getattr(c.photon, "mode", "fixture") == "live":
        from app.core.errors import ConflictError

        raise ConflictError(
            "disabled with live Photon; text the agent for real", error_code="SIMULATOR_DISABLED"
        )
    ch = channel_for_owner(db, user)
    if ch is None:
        from app.core.errors import ConflictError

        raise ConflictError("register your handle first", error_code="CHANNEL_REQUIRED")
    before = len(c.photon.outbox)
    c.photon.inject_inbound(ch.handle, body.text)
    res = poll_once(db, c)
    reply = c.photon.outbox[-1]["text"] if len(c.photon.outbox) > before else None
    return SimulatedTextResult(routed=res["routed"] > 0, reply=reply)


@owner_router.post("/channel/poll", response_model=PollResult)
def poll_channel(
    user: UUID = Depends(current_user), db: Session = Depends(db_session), c: Container = Depends(container)
):
    """Fetch new Photon messages now (for setups without a public webhook URL)."""
    return PollResult(**poll_once(db, c))


class OutreachAccepted(Contract):
    communication: CommunicationSession
    approval: ApprovalRequest


class StartCallBody(Contract):
    communication_id: UUID
    simulate: bool | None = None


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
    notify_owner(
        db,
        c,
        owner_id=project.owner_id,
        project_id=project.project_id,
        text=f"Your assistant wants to call {body.recipient.get('name', 'a broker')} about a property "
        f"({body.purpose}). Review and approve in the app: {c.settings.public_web_url}/activity",
        purpose="approval_request",
        idempotency_key=f"approval-notify:{appr.approval_id}",
    )
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
    # With the fixture voice provider no carrier exists, so the call is always a labeled simulation.
    simulated = True if getattr(c.telephony, "mode", "fixture") != "live" else bool(body.simulate)
    return CallOrchestrator(c).start(
        db,
        project_id=project.project_id,
        owner_id=project.owner_id,
        recipient=recipient,
        property_id=comm.property_id,
        approval_id=comm.approval_id,
        intro_recording_reference=comm.recording_reference,
        communication_id=comm.communication_id,
        simulated=simulated,
    )


@router.post("/calls/{call_id}/simulate", response_model=CallSession)
def simulate_call(
    call_id: UUID,
    body: SimulateBody,
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> CallSession:
    """Fixture mode only: play the broker side of a simulated call through the real event handlers."""
    call = repos.calls.require(db, call_id)
    if call.project_id != project.project_id:
        from app.core.errors import NotFoundError

        raise NotFoundError("call not found")
    return CallSimulator(c).run(db, call_id, body.owner_action, body.answers)


@router.get("/handoffs", response_model=list[OwnerHandoffRequest])
def list_handoffs(project: ProjectContext = Depends(owned_project), db: Session = Depends(db_session)):
    return repos.handoffs.list(db, project_id=project.project_id)


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
