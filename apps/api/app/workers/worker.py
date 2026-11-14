"""Background worker: pops event ids from Redis and runs subscribers in their own transaction.

Run: uv run python -m app.workers.worker
"""

from __future__ import annotations

import signal
from uuid import UUID

from app.core.config import get_settings
from app.core.logging import configure_logging, get_logger
from app.db.session import session_factory
from app.services.events import EventService
from app.workflows import property_analysis  # noqa: F401  (registers subscribers)

from .queue import EVENT_QUEUE, client

log = get_logger("worker")
_running = True


def _stop(*_):
    global _running
    _running = False


def process_one(event_id: UUID, attempt: int = 0) -> None:
    """Run subscribers for one event. Follow-up events use the container's EventService
    (redis mode -> enqueued again), so long chains never block a request."""
    inline = EventService("inline")
    db = session_factory()()
    try:
        ev = inline.get(db, event_id)
        if ev is None:
            # Producer may not have committed yet: retry a few times, then drop with a log line.
            if attempt < 5:
                client().rpush(EVENT_QUEUE, f"{event_id}:{attempt + 1}")
            else:
                log.warning("event not found", extra={"fields": {"event_id": str(event_id)}})
            return
        inline.dispatch(db, ev)
        db.commit()
    except Exception:
        db.rollback()
        log.exception("event processing failed", extra={"fields": {"event_id": str(event_id)}})
    finally:
        db.close()


def main() -> None:
    configure_logging(get_settings().log_level)
    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)
    r = client()
    log.info("worker started", extra={"fields": {"queue": EVENT_QUEUE}})
    while _running:
        item = r.blpop([EVENT_QUEUE], timeout=2)
        if item:
            raw, _, attempt = item[1].decode().partition(":")
            process_one(UUID(raw), int(attempt or 0))


if __name__ == "__main__":
    main()
