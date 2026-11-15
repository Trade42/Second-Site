# Rust backend image (apps/backend, binary `second-site-backend`). Build context: the repository
# root, as for the other images:  docker build -f infra/docker/backend.Dockerfile .
#
# Contains: the release binary, which has the SQL migrations (alembic 0001 and 0002) and the dev
#   seed project compiled in; the repository's fixtures/ tree, which the fixture providers read
#   at run time through FIXTURES_DIR (listings, permit snapshot, floor geometry); CA
#   certificates; a bash health probe; a non-root user; /data for uploaded objects.
# Does not contain: the Rust toolchain or sources, the Python app (Photon and the supervisor run
#   in the api image), curl, any .env file or secret (every setting comes from the environment,
#   see .env.example), or CoStar data (PROPERTY_PROVIDER_MODE=live needs a mounted kit and
#   COSTAR_KIT_DIR).

# ---- build ----
FROM rust:1.100.0-slim-bookworm AS build
WORKDIR /src/apps/backend
# rust-toolchain.toml is not copied on purpose: it names the moving "stable" channel and would make
# rustup download another toolchain; this image's pinned 1.100.0 builds instead.
# Layer 1, dependencies only (cached until a manifest or Cargo.lock changes): every workspace
# member gets its real Cargo.toml and an empty source. A new member needs a line here.
COPY apps/backend/Cargo.toml apps/backend/Cargo.lock ./
COPY apps/backend/crates/core/Cargo.toml crates/core/
COPY apps/backend/crates/domain/Cargo.toml crates/domain/
COPY apps/backend/crates/server/Cargo.toml crates/server/
COPY apps/backend/crates/nessie/Cargo.toml crates/nessie/
RUN for c in core domain server nessie; do mkdir -p crates/$c/src && : > crates/$c/src/lib.rs; done \
 && echo 'fn main() {}' > crates/server/src/main.rs \
 && cargo build --release --locked -p ss-server \
 && rm -rf crates/*/src
# Layer 2, the sources. fixtures/ goes to the same relative place as in the repository, because
# the dev seed route embeds fixtures/projects/facial_studio.json at compile time.
COPY apps/backend/crates crates
COPY fixtures /src/fixtures
# Copied files keep their build-context mtimes, which can be older than the stub build above, so
# touch them to make cargo rebuild the workspace crates.
RUN find crates -name '*.rs' -exec touch {} + \
 && cargo build --release --locked -p ss-server \
 && install -D -m 0755 target/release/second-site-backend /out/second-site-backend

# ---- run ----
FROM debian:bookworm-slim
# CA certificates: today's TLS clients (ureq for the provider adapters, sqlx) carry their own root
# store (webpki-roots), so the binary does not read these; they are here for clients that use the
# system store.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 secondsite \
 && useradd --system --uid 10001 --gid secondsite --home-dir /app --shell /usr/sbin/nologin secondsite \
 && install -d -o secondsite -g secondsite /app /data /data/objects
COPY --from=build /out/second-site-backend /usr/local/bin/second-site-backend
COPY apps/backend/scripts/healthcheck.sh /usr/local/bin/second-site-healthcheck
COPY fixtures /app/fixtures
ENV HOST=0.0.0.0 \
    PORT=8000 \
    STORE_BACKEND=postgres \
    FIXTURES_DIR=/app/fixtures \
    OBJECT_STORAGE_LOCAL_DIR=/data/objects
# /app holds no .env file, and the binary's second lookup (../../.env) resolves to /.env, which
# does not exist either: settings come only from the environment.
WORKDIR /app
USER secondsite
EXPOSE 8000
# No curl in the image: the probe uses bash's /dev/tcp (GET /health, expects 200 and "ok").
HEALTHCHECK --interval=10s --timeout=5s --start-period=15s --retries=6 \
  CMD ["bash", "/usr/local/bin/second-site-healthcheck"]
# DATABASE_URL is required (postgresql:// or postgresql+psycopg://); the schema is migrated to
# alembic head at start-up. `second-site-backend migrate` only migrates. STORE_BACKEND=memory runs
# without a database (nothing durable).
CMD ["second-site-backend"]
