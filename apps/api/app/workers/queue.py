"""Minimal Redis list queue for event dispatch (TASK_EXECUTION_MODE=redis). No extra framework."""

from __future__ import annotations

from uuid import UUID

import redis

from app.core.config import get_settings

EVENT_QUEUE = "secondsite:events"


def client() -> redis.Redis:
    return redis.Redis.from_url(get_settings().redis_url)


def enqueue_event(event_id: UUID) -> None:
    client().rpush(EVENT_QUEUE, str(event_id))
