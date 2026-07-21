# syntax=docker/dockerfile:1
#
# Production image for opn-core (Sprint 11 roadmap item 1) + the admin panel SPA
# (docs/opn-panel-roadmap.md P3). Three stages: a Node builder that produces the
# panel's static `dist/`, a Rust builder that produces the release binary, and a
# slim Debian runtime that carries only the binary, the panel, and a CA bundle.
# Built rarely — on release tags, not per-commit — so it favours simplicity over
# build-cache cleverness.
#
# BUILD CONTEXT IS THE REPO ROOT (compose sets `context: ..`), because the panel
# (opn-panel/) is a sibling of opn-core/, not a child. Each builder stage COPYs
# only its own subtree, so a panel edit never busts the cargo cache and vice
# versa. The root .dockerignore keeps target/, node_modules/, archive/, .git,
# and deploy/ out of the context.
#
# The Rust build needs NO database: the crate uses only runtime `sqlx::query(...)`,
# zero compile-time `query!` macros, so there is nothing to check against a live
# DB and no `.sqlx` offline cache to ship. `sqlx::migrate!("./migrations")` embeds
# the SQL into the binary at compile time, so the runtime needs no migrations dir.

# --- panel builder -------------------------------------------------------
# Static SPA (Vite + React). `npm ci` off the lockfile for a reproducible build,
# then `npm run build` (tsc -b && vite build) emits opn-panel/dist. package.json
# is copied first so a source-only edit reuses the cached npm install layer.
FROM node:22-slim AS panel
WORKDIR /panel
COPY opn-panel/package.json opn-panel/package-lock.json ./
RUN npm ci
COPY opn-panel/ .
RUN npm run build

# --- rust builder --------------------------------------------------------
# Pin the exact toolchain from rust-toolchain.toml (one source of truth for the
# compiler version; bump both together).
FROM rust:1.97.0-bookworm AS builder
WORKDIR /build
# ponytail: no manifest-first dependency-cache layer. The workspace makes the
# dummy-crate trick fiddly and this image is built only on release tags. If
# build time ever bites, add cargo-chef here — the upgrade path, not premature.
COPY opn-core/ .
RUN cargo build --release -p opn-core

# --- runtime -------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
# ca-certificates: the S3/MinIO SDK (and any TLS egress) needs the trust store.
# curl: the compose / Coolify healthcheck curls /healthz.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
# Run as a non-root, non-login system user: the server binds a high port and
# needs no root at runtime.
RUN useradd --system --uid 10001 --user-group --no-create-home opn
COPY --from=builder /build/target/release/opn-core /usr/local/bin/opn-core
# Panel SPA served off the admin bind (config.rs `panel_dir`). Baked as an image
# default so the image self-serves the panel with no compose config; it is only
# read when the admin surface is enabled (ADMIN_PASSWORD_HASH + ADMIN_JWT_SECRET
# both set), so it is inert on a deploy that leaves admin off.
COPY --from=panel /panel/dist /srv/panel
ENV ADMIN_PANEL_DIR=/srv/panel
USER opn
# The binary runs startup migrations (as OPN_MIGRATE_DATABASE_URL) then serves
# (crates/core/src/main.rs). Admin subcommands share the binary and run via
#   docker compose exec core opn-core admin <cmd> ...
ENTRYPOINT ["/usr/local/bin/opn-core"]
