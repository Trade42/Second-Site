"""Generic contract <-> row repository.

Each table stores the validated contract in `data` and copies a few fields into indexed columns.
Use `RecordRepository.for_(Model, Contract, id_field, columns)` rather than hand-written SQL in handlers.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable
from typing import Any, Generic, TypeVar
from uuid import UUID

from pydantic import BaseModel
from sqlalchemy import select
from sqlalchemy.orm import Session

from app.core.errors import ConflictError, NotFoundError

C = TypeVar("C", bound=BaseModel)


class RecordRepository(Generic[C]):
    def __init__(
        self,
        model: type,
        contract: type[C],
        id_field: str,
        columns: Callable[[C], dict[str, Any]] | None = None,
    ):
        self.model, self.contract, self.id_field = model, contract, id_field
        self.columns = columns or (lambda c: {})

    def _id(self, record: C) -> UUID:
        return getattr(record, self.id_field)

    def add(self, db: Session, record: C) -> C:
        row = self.model(id=self._id(record), data=record.model_dump(mode="json"), **self.columns(record))
        db.add(row)
        db.flush()
        return record

    def upsert(self, db: Session, record: C) -> C:
        row = db.get(self.model, self._id(record))
        if row is None:
            return self.add(db, record)
        row.data = record.model_dump(mode="json")
        for k, v in self.columns(record).items():
            setattr(row, k, v)
        row.record_version = (row.record_version or 1) + 1
        db.flush()
        return record

    def update_versioned(self, db: Session, record: C, expected_version: int) -> int:
        """Optimistic concurrency: fails if someone else wrote first."""
        row = db.get(self.model, self._id(record))
        if row is None:
            raise NotFoundError(f"{self.contract.__name__} not found")
        if row.record_version != expected_version:
            raise ConflictError(
                "stale record version",
                details={"expected": expected_version, "actual": row.record_version},
                error_code="STALE_VERSION",
            )
        row.data = record.model_dump(mode="json")
        for k, v in self.columns(record).items():
            setattr(row, k, v)
        row.record_version += 1
        db.flush()
        return row.record_version

    def get(self, db: Session, record_id: UUID) -> C | None:
        row = db.get(self.model, record_id)
        return self.contract.model_validate(row.data) if row else None

    def require(self, db: Session, record_id: UUID) -> C:
        rec = self.get(db, record_id)
        if rec is None:
            raise NotFoundError(f"{self.contract.__name__} {record_id} not found")
        return rec

    def row(self, db: Session, record_id: UUID):
        return db.get(self.model, record_id)

    def list(self, db: Session, *, order_by: Any = None, **filters: Any) -> list[C]:
        stmt = select(self.model)
        for k, v in filters.items():
            stmt = stmt.where(getattr(self.model, k) == v)
        stmt = stmt.order_by(order_by if order_by is not None else self.model.created_at)
        return [self.contract.model_validate(r.data) for r in db.scalars(stmt)]

    def add_all(self, db: Session, records: Iterable[C]) -> list[C]:
        return [self.add(db, r) for r in records]
