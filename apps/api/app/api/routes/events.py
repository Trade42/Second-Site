from __future__ import annotations

import asyncio
import json

from fastapi import APIRouter, Depends, Query
from fastapi.responses import StreamingResponse
from sqlalchemy.orm import Session

from app.api.deps import container, db_session, owned_project
from app.contracts import ActivityEvent, Contract, ProjectContext
from app.core.container import Container
from app.db.session import session_factory

router = APIRouter(prefix="/api/v1/projects/{project_id}", tags=["events"])


class EventPage(Contract):
    events: list[ActivityEvent]
    next_cursor: int


@router.get("/events", response_model=EventPage)
def list_events(
    after: int = Query(0, ge=0),
    limit: int = Query(200, ge=1, le=500),
    project: ProjectContext = Depends(owned_project),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> EventPage:
    rows = c.events.list(db, project.project_id, after, limit)
    return EventPage(events=[e for _, e in rows], next_cursor=rows[-1][0] if rows else after)


@router.get("/events/stream", response_class=StreamingResponse)
async def stream_events(
    after: int = Query(0, ge=0),
    project: ProjectContext = Depends(owned_project),
    c: Container = Depends(container),
):
    """Server-Sent Events. Polls the persisted event log (source of truth) every second."""

    async def gen():
        cursor = after
        for _ in range(3600):
            db = session_factory()()
            try:
                rows = c.events.list(db, project.project_id, cursor, 100)
            finally:
                db.close()
            for seq, ev in rows:
                cursor = seq
                yield f"id: {seq}\nevent: {ev.event_type.value}\ndata: {json.dumps(ev.model_dump(mode='json'))}\n\n"
            yield ": keepalive\n\n"
            await asyncio.sleep(1)

    return StreamingResponse(gen(), media_type="text/event-stream")
