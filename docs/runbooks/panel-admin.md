# Runbook: admin API / panel access

## Current state (verified)

The admin API (opn-panel-roadmap.md Sprints P0–P1) is a third axum router on
its own bind — login, tenant list/stats/audit, and the tenant lifecycle
mutations (create / rotate-key / freeze / unfreeze), every mutation writing an
`admin_audit` row. It is **feature-off by default**: unless `ADMIN_JWT_SECRET`
is set and non-empty ([config.rs](../../opn-core/crates/core/src/config.rs),
empty counts as absent), the router never starts and the deploy behaves exactly
as before. Auth is argon2id password → 30-min admin JWT (separate secret and
claim shape from tenant JWTs — neither verifies as the other). Login is
rate-limited by a single global bucket (fine for one operator on a private bind).

The password is **set on first launch through the panel**, not env, and stored
in the DB (`admin_credential`, migration 0016). The old `ADMIN_PASSWORD_HASH`
env var is gone: an argon2 PHC string is `$`-delimited, so compose/`.env`
interpolation shredded it and every login failed with *"not a valid argon2 PHC
string"*. `POST /admin/v1/setup` is **one-shot** — once the password exists it
409s, so the first setter owns the panel and everyone after needs that password.

In prod compose the container binds `0.0.0.0:9091` but the host publish is
`127.0.0.1:9091:9091` — **loopback only, no Traefik router, no TLS**. The SSH
tunnel is the front door and the transport security.

## Enable (one-time)

1. In Coolify's secret store set **one** var:
   - `OPN_ADMIN_JWT_SECRET` — `openssl rand -base64 48` (base64 has no `$`, so
     no interpolation trap; keep it secret — it signs admin JWTs)
2. Redeploy. Startup log confirms `admin panel SPA served off admin bind` and
   `admin router up` on 9091.
3. Open the tunnel + browse to the panel (see **Use**). First launch shows a
   **Set operator password** screen — choose a password (≥ 12 chars); it is
   hashed (argon2id) and stored in the DB, and you are logged straight in. From
   then on the panel shows the normal login.

## Use

Open the SSH tunnel first — it is the only door to the bind:

```bash
ssh -L 9091:127.0.0.1:9091 <prod-host>
```

**Panel (normal path):** browse to <http://localhost:9091/>. The admin bind
serves the built SPA (baked into the image at `/srv/panel`, P3) off the same
origin as the API, so the login page → tenant table → create/rotate/freeze all
work in the browser with no CORS. Log in with the admin password; the JWT lives
in browser memory only, so a reload lands back on the login page (by design —
no secret at rest).

**API (scripting / panel-down):** the same endpoints answer JSON directly:

```bash
curl -s -X POST localhost:9091/admin/v1/login -d '{"password":"…"}' \
  -H 'content-type: application/json'          # -> {token, expires_at}
curl -s localhost:9091/admin/v1/tenants -H "Authorization: Bearer <token>"
```

Raw API keys appear exactly once, in the create/rotate response body (browser:
the show-once modal; curl: the response). If the operator loses one, rotate —
it is not recoverable (only the sha256 hash is stored).

## Lost admin password

Not recoverable (argon2). Clear the stored credential so the panel returns to
its first-launch setup screen, then set a new password in the browser:

```bash
docker exec <core-container> \
  psql "$OPN_MIGRATE_DATABASE_URL" -c 'DELETE FROM admin_credential;'
# reload the panel → "Set operator password" again
```

No other data is affected. Existing admin JWTs die at their 30-min expiry (or
bounce the container to kill them now). Whoever reaches the bind first after the
delete sets the new password — fine, the bind is operator-only.

## Panel down / admin bind dead

The data plane is unaffected (separate router, separate task). Break-glass is
the CLI, same as before the panel existed:

```bash
docker exec <container-name> opn-core admin create-tenant --name x --new-world x
```

## Watch for

Repeated failed logins in the log (uniform `invalid credentials` + the login
rate-limit tripping) on a bind that should only ever see the operator —
that's someone on the host who shouldn't be. Treat as an incident
(incident-triage.md), not noise.
