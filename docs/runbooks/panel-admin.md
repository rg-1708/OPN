# Runbook: admin API / panel access

## Current state (verified)

The admin API (opn-panel-roadmap.md Sprints P0–P1) is a third axum router on
its own bind — login, tenant list/stats/audit, and the tenant lifecycle
mutations (create / rotate-key / freeze / unfreeze), every mutation writing an
`admin_audit` row. It is **feature-off by default**: unless BOTH
`ADMIN_PASSWORD_HASH` and `ADMIN_JWT_SECRET` are set and non-empty
([config.rs](../../opn-core/crates/core/src/config.rs), empty counts as
absent), the router never starts and the deploy behaves exactly as before.
Auth is argon2id password → 30-min admin JWT (separate secret and claim shape
from tenant JWTs — neither verifies as the other). Login is rate-limited by a
single global bucket (fine for one operator on a private bind).

In prod compose the container binds `0.0.0.0:9091` but the host publish is
`127.0.0.1:9091:9091` — **loopback only, no Traefik router, no TLS**. The SSH
tunnel is the front door and the transport security.

## Enable (one-time)

1. Generate the password hash (reads stdin — never puts the password in argv
   or shell history):
   ```bash
   # Coolify runs the stack from its own dir, so target the container directly
   # (-i: the command reads the password from stdin):
   docker ps --format '{{.Names}}' | grep -i core
   docker exec -i <container-name> opn-core admin hash-password
   # or locally: cargo run -p opn-core -- admin hash-password
   # type the password, press Enter, Ctrl-D
   ```
2. In Coolify's secret store set:
   - `OPN_ADMIN_PASSWORD_HASH` — the PHC string from step 1 (quote it; it
     contains `$`)
   - `OPN_ADMIN_JWT_SECRET` — `openssl rand -base64 48`
3. Redeploy. Startup log line confirms `admin api enabled` on 9091.

## Use

```bash
ssh -L 9091:127.0.0.1:9091 <prod-host>
curl -s -X POST localhost:9091/admin/v1/login -d '{"password":"…"}' \
  -H 'content-type: application/json'          # -> {token, expires_at}
curl -s localhost:9091/admin/v1/tenants -H "Authorization: Bearer <token>"
```

Raw API keys appear exactly once, in the create/rotate response body. If the
operator loses one, rotate — it is not recoverable (only the sha256 hash is
stored).

## Lost admin password

Not recoverable (argon2). Re-run step 1 with a new password, update
`OPN_ADMIN_PASSWORD_HASH`, redeploy. No data is affected; existing admin JWTs
die at their 30-min expiry (or bounce the container to kill them now).

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
