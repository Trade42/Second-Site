"""TaskService: long-running work is a TaskRecord; HTTP returns 202 + task reference."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any
from uuid import UUID

from sqlalchemy.orm import Session

from app.contracts import ActorType, EventType, Subsystem, TaskRecord, TaskStatus, TaskType
from app.repositories import registry as repos

from .events import EventService


class TaskService:
    def __init__(self, events: EventService):
        self.events = events

    def create(
        self,
        db: Session,
        project_id: UUID,
        task_type: TaskType,
        subsystem: Subsystem,
        input_reference: dict[str, Any] | None = None,
    ) -> TaskRecord:
        task = TaskRecord(
            project_id=project_id,
            task_type=task_type,
            assigned_subsystem=subsystem,
            input_reference=input_reference or {},
        )
        return repos.tasks.add(db, task)

    def _save(self, db: Session, task: TaskRecord) -> TaskRecord:
        repos.tasks.upsert(db, task)
        return task

    def _event(self, db, task: TaskRecord, et: EventType) -> None:
        self.events.emit(
            db,
            event_type=et,
            project_id=task.project_id,
            actor_type=ActorType.SYSTEM,
            actor_id="task-service",
            correlation_id=task.correlation_id,
            idempotency_key=f"task:{task.task_id}:{task.status.value}:{task.attempt_count}",
            payload={
                "task_id": str(task.task_id),
                "task_type": task.task_type.value,
                "status": task.status.value,
                "error_code": task.error_code,
            },
        )

    def run(self, db: Session, task: TaskRecord, fn: Callable[[TaskRecord], dict[str, Any]]) -> TaskRecord:
        """Execute inline. Worker mode calls the same function from app/workers/worker.py."""
        task = task.transition(TaskStatus.RUNNING).model_copy(
            update={"attempt_count": task.attempt_count + 1}
        )
        self._save(db, task)
        self._event(db, task, EventType.TASK_STARTED)
        try:
            with db.begin_nested():
                output = fn(task)
        except Exception as exc:  # noqa: BLE001
            task = task.transition(TaskStatus.FAILED).model_copy(
                update={"error_code": type(exc).__name__, "error_message": str(exc)[:500]}
            )
            self._save(db, task)
            self._event(db, task, EventType.TASK_FAILED)
            return task
        task = task.transition(TaskStatus.COMPLETED).model_copy(update={"output_reference": output})
        self._save(db, task)
        self._event(db, task, EventType.TASK_COMPLETED)
        return task
