from __future__ import annotations

import os
from logging.config import fileConfig

from alembic import context
from sqlalchemy import engine_from_config, pool

from app.core.config import get_settings
from app.db import models  # noqa: F401  (register tables)
from app.db.base import Base

config = context.config
if config.config_file_name is not None and not os.environ.get("ALEMBIC_SKIP_LOGGING"):
    fileConfig(config.config_file_name, disable_existing_loggers=False)

# Alembic keeps the URL in a configparser section, where '%' starts an interpolation. A percent-encoded URL (password,
# socket path, `options=-csearch_path%3D...`) is therefore stored with '%' doubled; get_main_option and get_section
# give back the URL exactly as given.
database_url = os.environ.get("DATABASE_URL") or get_settings().database_url
config.set_main_option("sqlalchemy.url", database_url.replace("%", "%%"))
target_metadata = Base.metadata

# Tables the Rust service (apps/backend) creates and migrates itself, outside alembic, on the same database
# (apps/backend/crates/server/migrations/rust-owned/). Autogenerate and `alembic check` must not propose dropping them,
# their indexes or their constraints. Exact names only, never a pattern, so that every other table alembic does not
# know is still reported. A listed name is compared as usual as soon as the Python models define that table; remove
# it from this list when an alembic revision adopts the table.
RUST_OWNED_TABLES = frozenset(
    {
        "nessie_simulation_records",  # Capital One Nessie sandbox ids and simulated payment attempts
    }
)


def include_object(obj, name, type_, reflected, compare_to) -> bool:
    """Autogenerate filter: skip a Rust-owned table and everything on it (columns, indexes, constraints)."""
    table = name if type_ == "table" else getattr(getattr(obj, "table", None), "name", None)
    return not (table in RUST_OWNED_TABLES and table not in target_metadata.tables)


def run_migrations_offline() -> None:
    context.configure(
        url=config.get_main_option("sqlalchemy.url"),
        target_metadata=target_metadata,
        literal_binds=True,
        dialect_opts={"paramstyle": "named"},
        compare_type=True,
        include_object=include_object,
    )
    with context.begin_transaction():
        context.run_migrations()


def run_migrations_online() -> None:
    connectable = engine_from_config(
        config.get_section(config.config_ini_section, {}), prefix="sqlalchemy.", poolclass=pool.NullPool
    )
    with connectable.connect() as connection:
        context.configure(
            connection=connection,
            target_metadata=target_metadata,
            compare_type=True,
            include_object=include_object,
        )
        with context.begin_transaction():
            context.run_migrations()


if context.is_offline_mode():
    run_migrations_offline()
else:
    run_migrations_online()
