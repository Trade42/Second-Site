#!/bin/sh
# One small machine: Postgres on a volume, then the API. Local socket only.
set -eu
PG_BIN=$(dirname "$(find /usr/lib/postgresql -name pg_ctl | head -1)")
export PATH="$PG_BIN:$PATH"
PGDATA=/data/pg
mkdir -p "$PGDATA" /var/run/postgresql
chown -R postgres:postgres "$PGDATA" /var/run/postgresql
if [ ! -f "$PGDATA/PG_VERSION" ]; then
  su postgres -c "initdb -D $PGDATA --auth=trust --username=postgres --encoding=UTF8 --locale=C"
fi
su postgres -c "pg_ctl -D $PGDATA -l /tmp/pg.log -o '-h 127.0.0.1' start"
i=0
while [ "$i" -lt 30 ]; do
  pg_isready -h 127.0.0.1 && break
  i=$((i + 1))
  sleep 1
done
su postgres -c "psql -h 127.0.0.1 -tc \"SELECT 1 FROM pg_roles WHERE rolname='secondsite'\"" | grep -q 1 \
  || su postgres -c "psql -h 127.0.0.1 -c \"CREATE USER secondsite SUPERUSER\""
su postgres -c "psql -h 127.0.0.1 -tc \"SELECT 1 FROM pg_database WHERE datname='secondsite'\"" | grep -q 1 \
  || su postgres -c "createdb -h 127.0.0.1 -O secondsite secondsite"
export DATABASE_URL="postgresql+psycopg://secondsite@127.0.0.1:5432/secondsite"
cd /repo/apps/api
uv run --no-sync alembic upgrade head
uv run --no-sync python /fly_seed.py
exec uv run --no-sync uvicorn app.main:app --host 0.0.0.0 --port 8000
