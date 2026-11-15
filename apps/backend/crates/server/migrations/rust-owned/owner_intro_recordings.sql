-- RUST-OWNED TABLE. This is NOT an alembic revision, and alembic does not know this table.
--
-- Created idempotently at start-up by the Rust service (`store::postgres::migrate`), exactly as
-- `nessie_simulation_records`; `alembic_version` is never touched and Python never reads it.
--
-- One row per owner introduction recording (`services::intro_recording`): status ACTIVE (at most
-- one per owner, kept by an advisory lock), REPLACED or REVOKED. `data` holds the record; the
-- audio itself is in object storage and is deleted when the record stops being ACTIVE.

CREATE TABLE IF NOT EXISTS owner_intro_recordings (
    id UUID NOT NULL,
    owner_id UUID NOT NULL,
    status VARCHAR(16) NOT NULL,
    link_token_sha256 VARCHAR(64),
    data JSONB NOT NULL,
    schema_version VARCHAR(16) NOT NULL,
    record_version INTEGER NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT now() NOT NULL,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT now() NOT NULL,
    CONSTRAINT pk_owner_intro_recordings PRIMARY KEY (id),
    CONSTRAINT fk_owner_intro_recordings_owner_id_users FOREIGN KEY(owner_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ix_owner_intro_recordings_owner_id ON owner_intro_recordings (owner_id);
CREATE INDEX IF NOT EXISTS ix_owner_intro_recordings_link_token_sha256 ON owner_intro_recordings (link_token_sha256);
