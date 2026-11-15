FROM python:3.12-slim
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 UV_COMPILE_BYTECODE=1
RUN apt-get update \
    && apt-get install -y --no-install-recommends postgresql postgresql-contrib \
    && rm -rf /var/lib/apt/lists/*
COPY --from=ghcr.io/astral-sh/uv:0.11.32 /uv /usr/local/bin/uv
WORKDIR /repo/apps/api
COPY apps/api/pyproject.toml apps/api/uv.lock apps/api/.python-version ./
RUN uv sync --frozen --no-dev --no-install-project
COPY apps/api ./
COPY fixtures /repo/fixtures
RUN uv sync --frozen --no-dev
COPY infra/docker/fly-start.sh /fly-start.sh
COPY infra/docker/fly_seed.py /fly_seed.py
RUN chmod +x /fly-start.sh
EXPOSE 8000
CMD ["/fly-start.sh"]
