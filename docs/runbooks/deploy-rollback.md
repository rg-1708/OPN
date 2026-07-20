# Runbook: deploy & rollback

Deploys opn-core to the Coolify host from
[docker-compose.prod.yml](../../opn-core/docker-compose.prod.yml), built by
[Dockerfile](../../opn-core/Dockerfile). Coolify runs Traefik as ingress; the
`traefik.*` labels in the compose wire TLS, HSTS, and sticky WebSockets. Verified
end to end on a dev host: the image builds, boots as `opn_app`, and `/healthz`
returns `200` (see the Sprint 11 part B reflection).

## 0. Required environment (Coolify secrets — never in the compose)

Every credential is a `${VAR}` in the compose, resolved from Coolify's
secret store. Set all of these on the app before the first deploy:

| Var | Consumed by | Notes |
|---|---|---|
| `OPN_DOMAIN` | Traefik router host, coturn realm | the public FQDN, e.g. `core.example.com` |
| `OPN_DB_MIGRATE_PASSWORD` | Postgres superuser + Core migrate URL | owner role `opn_migrate` |
| `OPN_DB_APP_PASSWORD` | initdb pre-seed + Core runtime URL | runtime role `opn_app` (see §2) |
| `OPN_S3_KEY` / `OPN_S3_SECRET` | MinIO root + Core S3 client | |
| `OPN_JWT_SECRET` | Core JWT signing (HS256) | rotate per [jwt-rotation.md](jwt-rotation.md) |
| `OPN_ICE_SERVERS` | Core → `calls.state` ICE list | JSON array of `RTCIceServer`; point at the coturn |
| `OPN_TURN_USER` / `OPN_TURN_PASSWORD` | coturn long-term creds | must match `OPN_ICE_SERVERS` |
| `OPN_RUST_LOG` | Core log filter | optional, defaults to `info` |

## 1. Traefik TLS notes (Coolify host)

**Do NOT hand-create a `tls-options.yaml` in `/data/coolify/proxy/dynamic/`.**
Verified on the first beta deploy: Traefik never loads operator-dropped files
there (not on watch, not on touch, not on proxy restart), and a router whose
`tls.options` reference cannot resolve does not fall back to defaults — it
fails to build entirely and every TLS handshake is reset. The
`tls.options=default@file` label was removed from the compose for this reason;
the router serves on Traefik's default TLS config. Re-adding a min-TLS-1.2
floor must go through Coolify's own proxy configuration (open item, see
[beta-release-findings.md](../beta-release-findings.md) §3).

The compose adds the HSTS middleware (`stsSeconds=31536000`, includeSubdomains,
forceSTSHeader) via plain docker-provider labels — those load fine. The
certresolver name (`letsencrypt`) and entrypoint name (`https`, Coolify's name
for :443 — stock `websecure` does not exist on a Coolify host) are confirmed
against the proxy's args; re-verify with
`docker inspect coolify-proxy --format '{{range .Args}}{{println .}}{{end}}'`
if the host changes.

## 2. First deploy (fresh Postgres volume)

The runtime role `opn_app` is least-privilege (NOSUPERUSER, NOBYPASSRLS) so RLS
is enforced in production. Migration `0001` seeds it with a **dev** password
(`'opn'`) and says production must set a real one out of band. That is done for
you by
[deploy/postgres/initdb/10-app-role-password.sh](../../opn-core/deploy/postgres/initdb/10-app-role-password.sh):
on a **fresh data dir only**, it pre-creates `opn_app` with `OPN_DB_APP_PASSWORD`
before Core migrates; `0001`'s `IF NOT EXISTS` guard then skips re-seeding and
just applies its GRANTs. So on first deploy there is **no manual `ALTER ROLE`
step** — set `OPN_DB_APP_PASSWORD` and deploy.

1. Set every var in §0 as a Coolify secret.
2. Do §1 (Traefik TLS options) once.
3. Point the Coolify app at `opn-core/docker-compose.prod.yml`, set the domain to
   `OPN_DOMAIN` (Coolify provisions the LE cert).
4. Deploy. Coolify builds the image, starts the stack, and gates routing on the
   `core` healthcheck (`curl /healthz`). Rollout completes only when Core is
   healthy.

> The `initdb` pre-seed runs **only** when the `pgdata` volume is empty. If you
> ever change `OPN_DB_APP_PASSWORD` on an existing volume, the seed will NOT
> re-run — rotate it by hand: `ALTER ROLE opn_app PASSWORD '<new>'` as the owner,
> then update the secret and redeploy.

## 3. Update deploy

1. Merge to the release branch / push the tag.
2. Coolify rebuilds the image and does a healthz-gated rolling replace. Migrations
   are forward-only ([roadmap §8](../opn-core-roadmap.md)); Core runs any new ones
   at startup as the owner role before serving.
3. Verify (§5).

## 4. Rollback

**Binary rollback is safe only within the same migration version.** Migrations
are forward-only and not auto-reversible: if the bad deploy already applied a new
migration that changed a schema the old binary doesn't expect, rolling the binary
back can break it. Default to **roll forward** (fix + redeploy). Roll the binary
back only when you have confirmed the deploy did not add a migration.

- **Coolify:** open the app → Deployments → redeploy the previous successful
  deployment (or the previous git tag). Healthz gates the rollback the same way.
- **Manual (host):** repin Core to a known-good image and replace just that
  service, leaving the stores up:
  ```bash
  # from the app dir, with the prod env loaded
  docker compose -f docker-compose.prod.yml up -d --no-deps --force-recreate core
  ```
- If Core is crash-looping on startup, check the log for a migration or config
  error before rolling back — a missing/renamed env var (§0) looks like a bad
  build but is fixed by setting the secret, not reverting.

## 5. Verify after deploy

```bash
# through the domain (Traefik) — confirms TLS + routing + the live build
curl -s https://$OPN_DOMAIN/healthz | jq .
# {"status":"ok","contracts_version":"…","core_version":"…"}
```

- `status:"ok"` → PG (as `opn_app`) and Redis both live.
- `contracts_version` / `core_version` confirm exactly which build is serving —
  check these right after a deploy or rollback.
- `503` blocks rollout by design; which store is down is in the log
  (`healthz failing`, fields `pg_ok`/`redis_ok`), not the body — see
  [incident-triage.md](incident-triage.md).
- Metrics are internal-only (no host port, no Traefik router): scrape
  `core:9090/metrics` from Prometheus on the compose network, or
  `docker compose exec core curl -s localhost:9090/metrics`.

Admin commands (e.g. tenant creation, `admin unfreeze`) run through the same
binary in the running container:

```bash
docker compose -f docker-compose.prod.yml exec core opn-core admin create-tenant --name <t> --new-world <w>
```
