-- Alembic revision 0002 (owner channels), generated verbatim by
--   (cd apps/api && .venv/bin/alembic upgrade head --sql)
-- without the BEGIN/COMMIT and alembic_version statements, which the Rust migration runner
-- issues itself exactly as alembic does. Do not edit: regenerate from the alembic revision.

CREATE TABLE owner_channels (
    id UUID NOT NULL, 
    owner_id UUID NOT NULL, 
    platform VARCHAR(32) NOT NULL, 
    handle VARCHAR(320) NOT NULL, 
    status VARCHAR(32) NOT NULL, 
    data JSONB NOT NULL, 
    schema_version VARCHAR(16) NOT NULL, 
    record_version INTEGER NOT NULL, 
    created_at TIMESTAMP WITH TIME ZONE DEFAULT now() NOT NULL, 
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT now() NOT NULL, 
    CONSTRAINT pk_owner_channels PRIMARY KEY (id), 
    CONSTRAINT fk_owner_channels_owner_id_users FOREIGN KEY(owner_id) REFERENCES users (id) ON DELETE CASCADE, 
    CONSTRAINT uq_owner_channels_platform UNIQUE (platform, handle)
);

CREATE INDEX ix_owner_channels_owner_id ON owner_channels (owner_id);
