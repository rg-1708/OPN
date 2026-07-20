# syntax=docker/dockerfile:1
#
# Production image for opn-core (Sprint 11 roadmap item 1). Two stages: a Rust
# builder that produces the release binary, and a slim Debian runtime that
# carries only the binary + a CA bundle. Built rarely — on release tags, not
# per-commit — so it favours simplicity over build-cache cleverness.
#
# The build needs NO database: the crate uses only runtime `sqlx::query(...)`,
# zero compile-time `query!` macros, so there is nothing to check against a live
# DB and no `.sqlx` offline cache to ship. It is a pure offline `cargo build`.
# `sqlx::migrate!("./migrations")` embeds the SQL into the binary at compile
# time, so the runtime image needs no migrations directory either.

# --- builder -------------------------------------------------------------
# Pin the exact toolchain from rust-toolchain.toml (one source of truth for the
# compiler version; bump both together).
FROM rust:1.97.0-bookworm AS builder
WORKDIR /build
# ponytail: no manifest-first dependency-cache layer. The workspace makes the
# dummy-crate trick fiddly and this image is built only on release tags. If
# build time ever bites, add cargo-chef here — the upgrade path, not premature.
COPY . .
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
USER opn
# The binary runs startup migrations (as OPN_MIGRATE_DATABASE_URL) then serves
# (crates/core/src/main.rs). Admin subcommands share the binary and run via
#   docker compose exec core opn-core admin <cmd> ...
ENTRYPOINT ["/usr/local/bin/opn-core"]
