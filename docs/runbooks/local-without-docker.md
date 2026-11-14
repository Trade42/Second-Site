# Running locally without Docker

```bash
# PostgreSQL 16 + Redis (Debian/Ubuntu)
sudo apt-get install -y postgresql redis-server
sudo service postgresql start && sudo service redis-server start
sudo -u postgres psql -c "CREATE USER secondsite WITH PASSWORD 'secondsite' CREATEDB;"
sudo -u postgres createdb -O secondsite secondsite
sudo -u postgres createdb -O secondsite secondsite_test
```
macOS: `brew install postgresql@16 redis && brew services start postgresql@16 redis`, then the same `createuser/createdb`.

Then `scripts/bootstrap.sh --no-docker` and `scripts/dev.sh`.
Redis is only needed with `TASK_EXECUTION_MODE=redis` (+ `uv run python -m app.workers.worker`).
