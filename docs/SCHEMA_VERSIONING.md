# Schema versioning

- Global `SCHEMA_VERSION = "1.0"` (`contracts/common.py`), stamped on events and in `openapi.json` (`info.x-schema-version`).
- **Backward-compatible** (no version bump): add an optional field with a default/`None`; add an enum member that consumers treat as unknown; add a new contract or event type (with payload model).
- **Breaking** (bump to `1.1`/`2.0`, coordinate): rename/remove a field, tighten validation, change units, change an event payload's required fields, change enum meaning.
- Breaking changes: add the new shape alongside the old (e.g. `PermitRecordV2` or new event type), migrate producers, then consumers, then remove the old one in a later PR.
- DB: contract docs live in JSONB `data`, so optional additions need no migration; new indexed columns need an Alembic revision (`uv run alembic revision --autogenerate -m "..."`, then `alembic check`).
- Every contract change: update model → `scripts/generate-contracts.sh` → update fixtures/tests → post on the board (who is affected) → PR to `main` → affected branches merge `main`.
