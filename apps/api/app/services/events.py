"""EventService: persist ActivityEvents (idempotent) and dispatch to subscribers.

Loop safety:
- `idempotency_key` is unique: re-emitting the same logical event is a no-op.
- Handlers emit their result with key "<handler>:<cause_event_id>", so redelivery never re-runs a handler.
- Causation chains deeper than MAX_CAUSATION_DEPTH are refused.
Dispatch is inline (same transaction) by default; TASK_EXECUTION_MODE=redis enqueues event ids for the worker.
"""

from __future__ import annotations

from collections import defaultdict
from collections.abc import Callable
from typing import Any
from uuid import UUID, uuid4

from sqlalchemy import select
from sqlalchemy.orm import Session

from app.contracts import ActivityEvent, ActorType, EventType
from app.core.logging import get_logger
from app.db import models as m

log = get_logger(__name__)
MAX_CAUSATION_DEPTH = 12

Handler = Callable[[Session, ActivityEvent], None]
_SUBSCRIBERS: dict[EventType, list[tuple[str, Handler]]] = defaultdict(list)


def subscribe(event_type: EventType, name: str) -> Callable[[Handler], Handler]:
    def deco(fn: Handler) -> Handler:
        if all(n != name for n, _ in _SUBSCRIBERS[event_type]):
            _SUBSCRIBERS[event_type].append((name, fn))
        return fn

    return deco


def subscribers(event_type: EventType) -> list[tuple[str, Handler]]:
    return list(_SUBSCRIBERS.get(event_type, []))


class EventService:
    def __init__(self, dispatch_mode: str = "inline", enqueue: Callable[[UUID], None] | None = None):
        self.dispatch_mode = dispatch_mode
        self.enqueue = enqueue

    def exists(self, db: Session, idempotency_key: str) -> bool:
        return (
            db.scalar(
                select(m.ActivityEventRow.id).where(m.ActivityEventRow.idempotency_key == idempotency_key)
            )
            is not None
        )

    def depth(self, db: Session, causation_id: UUID | None) -> int:
        d = 0
        while causation_id is not None and d <= MAX_CAUSATION_DEPTH:
            row = db.get(m.ActivityEventRow, causation_id)
            if row is None:
                break
            causation_id = row.causation_id
            d += 1
        return d

    def emit(
        self,
        db: Session,
        *,
        event_type: EventType,
        project_id: UUID,
        payload: dict[str, Any],
        actor_type: ActorType,
        actor_id: str,
        idempotency_key: str,
        correlation_id: UUID | None = None,
        causation: ActivityEvent | None = None,
        dispatch: bool = True,
    ) -> ActivityEvent | None:
        """Returns the new event, or None when the idempotency key was already used."""
        if self.exists(db, idempotency_key):
            return None
        if causation is not None and self.depth(db, causation.event_id) >= MAX_CAUSATION_DEPTH:
            raise RuntimeError(f"causation chain too deep for {event_type}")
        event = ActivityEvent(
            event_type=event_type,
            project_id=project_id,
            correlation_id=correlation_id or (causation.correlation_id if causation else uuid4()),
            causation_id=causation.event_id if causation else None,
            idempotency_key=idempotency_key,
            actor_type=actor_type,
            actor_id=actor_id,
            payload=payload,
        )
        db.add(
            m.ActivityEventRow(
                id=event.event_id,
                project_id=project_id,
                event_type=event_type.value,
                correlation_id=event.correlation_id,
                causation_id=event.causation_id,
                idempotency_key=idempotency_key,
                envelope=event.model_dump(mode="json"),
            )
        )
        db.flush()
        if dispatch:
            self.dispatch(db, event)
        return event

    def dispatch(self, db: Session, event: ActivityEvent) -> None:
        if self.dispatch_mode == "redis" and self.enqueue is not None:
            self.enqueue(event.event_id)
            return
        for name, handler in subscribers(event.event_type):
            try:
                handler(db, event)
            except Exception:  # noqa: BLE001 — a consumer failure must not lose the producer's event
                log.exception(
                    "event handler failed", extra={"fields": {"handler": name, "event": str(event.event_id)}}
                )
                raise

    def list(
        self, db: Session, project_id: UUID, after_seq: int = 0, limit: int = 200
    ) -> list[tuple[int, ActivityEvent]]:
        rows = db.scalars(
            select(m.ActivityEventRow)
            .where(m.ActivityEventRow.project_id == project_id, m.ActivityEventRow.seq > after_seq)
            .order_by(m.ActivityEventRow.seq)
            .limit(limit)
        )
        return [(r.seq, ActivityEvent.model_validate(r.envelope)) for r in rows]

    def get(self, db: Session, event_id: UUID) -> ActivityEvent | None:
        row = db.get(m.ActivityEventRow, event_id)
        return ActivityEvent.model_validate(row.envelope) if row else None
