# Runbook: security pass

Sprint 11 item 8. Records the standing security posture and the audits that gate
a release: secrets never in logs, secrets never in the repo, TLS/HSTS at the
edge, and a clean dependency-advisory gate.

## 1. Secrets never in logs (audited — clean)

Full read of all 36 log-macro call sites in `crates/core` (Sprint 11 part B).
**No secret reaches a log or trace today.** Strongest reasons:

- **No `#[instrument]` anywhere in the tree** — the most common accidental leak
  (it auto-records every function argument as a span field) does not exist here.
- `Config` (`config.rs:7`) and `Claims` (`infra/auth.rs:28`) derive `Debug` but
  are **never** `{:?}`-formatted or logged; only individual non-secret fields
  (`bind`, `metrics_bind`) are read — e.g. startup log `main.rs:93`.
- JWT verify failure **drops** the decode error with `|_|` (`infra/auth.rs:114`),
  so a bad/forged token is never echoed; the error log there carries a DB error,
  not the token or secret.
- The API-key and Bearer extractors reject with **static strings**
  (`http/tenant.rs:39`, `http/auth.rs:28`) — the presented credential is never
  interpolated into a response or log; the key is hashed and bound as a query
  param, never printed.
- Config `req`/`parse` errors interpolate only the **env-var name**, never its
  value (`config.rs:60`).
- The tenant API key is emitted exactly once, to **stdout**, by the admin CLI
  (`admin.rs:110`) — by design (printed once, never stored), not a tracing sink.
- `infra/s3.rs` builds presigned URLs but contains **zero** log macros; the media
  path logs the media id, not the URL.

**Latent risk to hold (not a leak today):** `Config` derives `Debug`
(`config.rs:7`). A future stray `debug!(?cfg)` would dump every DSN and secret at
once. Cheap hardening: a manual redacting `Debug` impl for `Config`. Left as a
follow-up — nothing triggers it now.

**Re-run before each release:** `grep -rn '#\[instrument' crates/core/src` must
stay empty, or every hit must `skip`/`skip_all` its secret-bearing args; and
`grep -rn 'debug!(?cfg\|info!(?cfg\|?.*secret\|?.*password' crates/core/src` must
be empty.

## 2. Secrets never in the repo

- Every credential in
  [docker-compose.prod.yml](../../opn-core/docker-compose.prod.yml) is a `${VAR}`
  resolved from Coolify's secret store — no real password is committed.
- Postgres, Redis, and MinIO publish **no host ports** in the prod compose; they
  are reachable only on the internal compose network. The runtime DB role
  `opn_app` is NOSUPERUSER/NOBYPASSRLS, so RLS is enforced even from inside.
- The metrics listener (`OPN_METRICS_BIND`) is internal-only: no host port, no
  Traefik router.

## 3. TLS / HSTS at the edge

Configured in the compose Traefik labels + one operator-side dynamic-config
block; see [deploy-rollback.md §1](deploy-rollback.md). Summary:

- HTTPS-only router, LE cert via `certresolver`.
- Min **TLS 1.2** via the `default@file` tls-options block.
- **HSTS** middleware: `max-age=31536000`, includeSubdomains, forceSTSHeader.

## 4. Dependency advisories

`cargo deny check` (licenses + advisories) is a CI gate since Sprint 9
([ci.yml](../../.github/workflows/ci.yml) `cargo-deny` job) — green. Re-runs on
every push; a new advisory fails the build.

## 5. Operator / deferred

- A real HSTS/TLS handshake (`testssl.sh` or a browser check against the live
  domain) is an operator step once the cert is provisioned.
- JWT-secret rotation procedure: [jwt-rotation.md](jwt-rotation.md) (single-secret
  swap now; dual-secret overlap marked planned there).
