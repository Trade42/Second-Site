from __future__ import annotations

from datetime import datetime
from uuid import UUID

from sqlalchemy import JSON, DateTime, Integer, MetaData, String, func
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column

# JSONB on Postgres, JSON elsewhere (SQLite in fast unit tests).
JsonDoc = JSON().with_variant(JSONB(), "postgresql")

NAMING = {
    "ix": "ix_%(column_0_label)s",
    "uq": "uq_%(table_name)s_%(column_0_name)s",
    "ck": "ck_%(table_name)s_%(constraint_name)s",
    "fk": "fk_%(table_name)s_%(column_0_name)s_%(referred_table_name)s",
    "pk": "pk_%(table_name)s",
}


class Base(DeclarativeBase):
    metadata = MetaData(naming_convention=NAMING)


class TimestampMixin:
    created_at: Mapped[datetime] = mapped_column(
        DateTime(timezone=True), server_default=func.now(), nullable=False
    )
    updated_at: Mapped[datetime] = mapped_column(
        DateTime(timezone=True), server_default=func.now(), onupdate=func.now(), nullable=False
    )


class DocumentMixin(TimestampMixin):
    """Rows store the validated contract document in `data`, plus indexed columns for querying.

    `data` is always produced by `Contract.model_dump(mode="json")` — never raw provider payloads
    (those go to `property_sources.raw_payload`).
    """

    data: Mapped[dict] = mapped_column(JsonDoc, nullable=False)
    schema_version: Mapped[str] = mapped_column(String(16), nullable=False, default="1.0")
    record_version: Mapped[int] = mapped_column(Integer, nullable=False, default=1)


__all__ = ["Base", "DocumentMixin", "TimestampMixin", "JsonDoc", "UUID"]
