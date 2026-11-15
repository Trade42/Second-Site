-- RUST-OWNED TABLE. This is NOT an alembic revision, and alembic does not know this table.
--
-- The Rust service creates it idempotently at start-up (`store::postgres::migrate`), after the
-- alembic-compatible runner and under the same per-schema migration lock, and never touches
-- `alembic_version`, so the Python app's `alembic upgrade head` keeps working on the same
-- database. Python never reads this table. A matching alembic revision is a later team decision
-- (to be proposed on the team board).
--
-- One row per project for the Capital One Nessie sandbox ids (kind 'sandbox', record_key
-- 'sandbox'), and one row per property and plan item for a simulated payment attempt (kind
-- 'payment_attempt', record_key '{property_id}:{plan_item_id}'). `data` holds the validated
-- document, as in every other document table. Mock money only.

CREATE TABLE IF NOT EXISTS nessie_simulation_records (
    id UUID NOT NULL,
    project_id UUID NOT NULL,
    kind VARCHAR(32) NOT NULL,
    record_key VARCHAR(255) NOT NULL,
    approval_id UUID,
    data JSONB NOT NULL,
    schema_version VARCHAR(16) NOT NULL,
    record_version INTEGER NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT now() NOT NULL,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT now() NOT NULL,
    CONSTRAINT pk_nessie_simulation_records PRIMARY KEY (id),
    CONSTRAINT fk_nessie_simulation_records_project_id_projects FOREIGN KEY(project_id) REFERENCES projects (id) ON DELETE CASCADE,
    CONSTRAINT uq_nessie_simulation_records_project_id UNIQUE (project_id, kind, record_key)
);

CREATE INDEX IF NOT EXISTS ix_nessie_simulation_records_approval_id ON nessie_simulation_records (approval_id);
