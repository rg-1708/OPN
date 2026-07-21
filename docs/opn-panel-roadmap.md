# OPN Panel — Admin Dashboard Roadmap (draft v0.1)

Companion to [OPN-CORE.md](OPN-CORE.md). Same reading rules: sprints
scope-bound, each has Goal / Depends on / Work items / Test plan / Exit
criteria; Core-side changes go through a CDR first.

**Status (2026-07-21): P0–P3 shipped.** Admin bind, login, read endpoints,
audit table, and full tenant/key mutations (P0–P1); the Vite/React panel SPA
served off the admin bind with login, tenant table, show-once key modal,
rotate/freeze flows, stats tiles, and audit view (P2); prod wiring — panel
built in the Dockerfile and baked at `/srv/panel`, compose builds from the repo
root, runbook covers browser + CLI access (P3). Playwright smoke
(`opn-panel/tests/smoke.spec.ts`) runs against a live dev stack.

## Why this doc exists — and the anti-goal it overrides

The Core roadmap (Sprint 11) rejected an HTTP admin route: operator actions
were CLI break-glass (`opn-core admin create-tenant`, `unfreeze`) and
"no dashboards-for-dashboards". That held while the operator was also the
developer with shell access. It stops holding once tenants/API keys need
routine management — issuing keys for client apps over SSH does not scale
past one person.

**This doc supersedes that anti-goal, deliberately and narrowly**: one admin
surface, for the operator, to manage tenants and credentials. It is not a
tenant-facing portal and not a metrics product (Grafana/pg exist).

`opn-web-conference` is unaffected — it stays a POC client of the data
plane. The panel is a separate app talking to a separate, private API.

## Architecture

Two pieces, one new deploy surface:

1. **Admin API in Core** — a third axum router (`admin_router`) beside
   `app_router` and `metrics_router`, on its **own bind** (default
   `127.0.0.1:9091`, never the public bind). Same binary, same DB pool
   pattern; endpoints run statements that require the elevated role the CLI
   already uses. Reuses Core's existing auth/argon2/jsonwebtoken machinery.
2. **`opn-panel/`** — new top-level directory: Vite + React + TS SPA
   (matches the web template's toolchain). Built output served as static
   files by the admin bind itself — no extra web server, no CORS, one
   origin. Dev mode: Vite proxy to the admin bind.

Reaching it in prod: the admin bind stays loopback/VPN-only; operator
reaches it via SSH tunnel or WireGuard. TLS/exposure is the tunnel's job,
not Core's.

### Admin authentication (v1)

Single admin principal. The admin surface is enabled by `ADMIN_JWT_SECRET`
alone (base64, safe in env). The **password is set on first launch through the
panel**, not env, and stored argon2id-hashed in the DB (`admin_credential`,
migration 0016). This supersedes the original `ADMIN_PASSWORD_HASH`-in-env
design: an argon2 PHC string is `$`-delimited, so compose/`.env` interpolation
shredded it and every login failed — the env path was a footgun the operator
could not get right. Setup (`POST /admin/v1/setup`) is **one-shot**: once a
credential row exists it 409s, so the first setter owns the panel and everyone
after needs that password. It is unauthed but safe because the bind is private
(loopback/tunnel) — the same trust boundary env assumed. Login verifies against
the stored hash and mints a short-lived admin JWT (existing jsonwebtoken
plumbing, separate signing key `ADMIN_JWT_SECRET`, TTL 30 min) which the SPA
holds in memory and sends as `Authorization: Bearer`. Rate-limited login +
setup, constant failure timing, all admin mutations audit-logged. No
multi-admin, no roles, no TOTP in v1 — the bind is private and there is one
operator. Gated below.

### Admin API surface (v1)

| Endpoint | Action |
|---|---|
| `GET /admin/v1/status` | unauthed — `{configured}`: is a password set yet (setup vs login screen) |
| `POST /admin/v1/setup` | unauthed, one-shot — set the first-launch password → admin JWT (auto-login) |
| `POST /admin/v1/login` | password → admin JWT |
| `GET /admin/v1/tenants` | list: name, created, frozen, key fingerprint, last session |
| `POST /admin/v1/tenants` | create tenant → **raw API key in response, shown once** |
| `POST /admin/v1/tenants/{id}/rotate-key` | new key (shown once), old hash invalid immediately |
| `POST /admin/v1/tenants/{id}/freeze` / `unfreeze` | parity with CLI unfreeze |
| `DELETE /admin/v1/tenants/{id}` | hard-delete tenant + key (irreversible); 409 if it has live sessions; audit trail survives |
| `GET /admin/v1/stats` | counts: tenants, live sessions, active calls, msgs/24h |
| `GET /admin/v1/audit` | admin action log, newest first |

Key rotation is new capability (CLI has only create). Rotation is
immediate-cut v1; grace-period dual-key is gated.

## Cross-cutting rules (every sprint)

1. **Admin surface never on the public bind.** Startup refuses a config
   where admin bind equals public bind.
2. **Raw API keys appear exactly once** — in the create/rotate response.
   Never logged, never in audit rows (fingerprint = first 8 hex of hash).
3. **Every mutation writes an audit row** (who is implicit v1; what, when,
   target tenant).
4. **CLI keeps working.** Panel is a second door, not a replacement —
   break-glass survives a dead panel.
5. Panel SPA holds no secrets at rest; JWT in memory only, re-login on
   refresh is acceptable.

## Sprint sequence at a glance

| # | Name | Depends on | Delivers |
|---|---|---|---|
| P0 | Admin API read-only + auth | — | **done** — admin bind, login, list/stats endpoints, audit table |
| P1 | Mutations | P0 | **done** — create / rotate-key / freeze / unfreeze, audit rows |
| P2 | Panel SPA | P1 | **done** — login page, tenant table, create/rotate flows with show-once key modal, stats header, audit view, static build served by admin bind |
| P3 | Ops polish | P2 | **done** — panel build step in Dockerfile, prod compose entry (admin bind loopback + panel baked), runbook (tunnel + browser/CLI access) |

## Sprint P2 — Panel SPA

**Goal**: the SaaS-like dashboard — a human does everything P1 can, without
curl.

Work items: `opn-panel/` scaffold (Vite + React + TS, strict); login screen;
tenant table (name, status, key fingerprint, last session); create-tenant
flow with **show-once key modal** (copy button, explicit "I saved it"
confirm); rotate with confirm dialog; freeze/unfreeze toggle; stats header
tiles; audit log view; static build served by admin bind.

Test plan: Playwright smoke against dev stack — login, create tenant, see
key once, key absent after reload, rotate, freeze.
Exit: operator manages a tenant end to end in the browser on the dev stack.

## Sprint P3 — Ops polish

**Goal**: runs on the prod host unattended.

Work items: prod compose entry (admin bind on loopback only); build step for
panel in Dockerfile; `runbooks/panel-admin.md` (tunnel setup, lost admin
password recovery = re-hash env + restart, panel-down fallback = CLI);
alert on repeated failed logins.

Exit: panel reachable via tunnel on prod host, runbook validated by
following it cold.

## Gated (build on demand, not before)

Multi-admin + roles (needs real admin identity model); TOTP/2FA; key
rotation grace period (dual-key window); tenant-facing self-service portal;
per-tenant config editing (ICE servers, rate limits) — config stays in env
until a second operator actually needs runtime editing; charts/metrics
beyond the stat tiles (Grafana owns that).
