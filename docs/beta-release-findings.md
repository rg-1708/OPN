# Beta release findings — first Coolify deploy

Findings from the first real deploy of `opn-core` to the Coolify host
(2026-07-20), deployed from [docker-compose.prod.yml](../opn-core/docker-compose.prod.yml)
per [deploy-rollback.md](runbooks/deploy-rollback.md). End state: stack healthy,
`/healthz` returning `200` through Traefik with a real Let's Encrypt cert over
HTTP/2. Everything below is what the runbook did not predict.

## 1. Coolify does NOT substitute `${VAR}` inside `traefik.*` labels

Coolify resolves `${VAR}` in `environment:` and `command:` (Postgres booted with
the real passwords) but passes **labels through verbatim**. Traefik received the
literal rule `` Host(`${OPN_DOMAIN}`) `` and asked Let's Encrypt for a cert on
the literal string:

```
acme: error: 400 ... Cannot issue for "${opn_domain}": Domain name contains an invalid character
```

**Fix:** hardcode the FQDN in the router rule label. The compose now carries the
real domain; changing domains means editing the compose, not just the secret.
`${OPN_DOMAIN}` still works fine in `environment:` and in coturn's `command:`.

## 2. Coolify's Traefik names entrypoints `http`/`https`, not `web`/`websecure`

Stock-Traefik naming (`websecure`) fails on a Coolify host:

```
ERR EntryPoint doesn't exist entryPointName=websecure routerName=opn-core@docker
```

Confirmed from the proxy args (`docker inspect coolify-proxy`):
`--entrypoints.http.address=:80`, `--entrypoints.https.address=:443`. The
certresolver name `letsencrypt` in our labels does match Coolify's
(`--certificatesresolvers.letsencrypt...`), so only the entrypoint needed
renaming.

## 3. Hand-dropped dynamic-config files in `/data/coolify/proxy/dynamic/` never load

The runbook's §1 (create `tls-options.yaml` with `minVersion: VersionTLS12`)
does not work on a Coolify host. The file was present, valid, and visible inside
the container at the watched path (`/traefik/dynamic/`, `watch=true`), and it
survived a proxy restart — but Traefik never registered the options block:

```
ERR error="building router handler: unknown TLS options: default@file"
```

Touching the file did not trigger a reload; neither did `docker restart
coolify-proxy`. A router whose `tls.options` reference cannot resolve does not
fall back to defaults — it **fails to build entirely**, and every TLS handshake
is reset (curl: `unexpected eof while reading`). So a "hardening" label took the
whole route down.

**Fix:** dropped the `tls.options=default@file` label. The router now serves on
Traefik's default TLS config. Deferred: re-add the min-TLS-1.2 floor through
Coolify's own TLS/proxy configuration rather than a raw file (tracked as a
`ponytail:` comment in the compose).

## 4. Coolify env var UI rejects cross-references

Setting `OPN_ICE_SERVERS` to JSON containing `${OPN_DOMAIN}`/`${OPN_TURN_*}`
refs fails the deploy with:

```
Invalid expression; variable cycle not allowed for OPN_DOMAIN
```

**Fix:** store `OPN_ICE_SERVERS` as fully-resolved literal JSON. Generate it
with `jq` so escaping is safe:

```bash
jq -nc --arg d "<domain>" --arg u "<turn-user>" --arg p "<turn-pass>" \
  '[{urls:"stun:\($d):3478"},{urls:"turn:\($d):3478",username:$u,credential:$p}]'
```

Consequence: rotating the TURN password or changing the domain means
regenerating this var by hand — it does not follow the other secrets.

## 5. Coolify UI settings that mattered

- **Build Pack** must be `Docker Compose` (not the default Nixpacks); Base
  Directory `/opn-core`, Docker Compose Location `/docker-compose.prod.yml`.
- **"Preserve Repository During Deployment" must be checked.** The compose
  bind-mounts `./deploy/postgres/initdb` into Postgres at runtime; without
  preservation the mount source is cleaned after build and the `opn_app`
  role pre-seed never runs.
- **Coolify's per-service "Domains" fields stay empty for every service.**
  Routing is fully defined by the compose's `traefik.*` labels; filling the
  core domain in the UI would create a duplicate router. minio/createbucket/
  coturn are internal/non-HTTP by design.
- Coolify does not auto-scrape `${VAR}` refs from a compose; every variable
  from runbook §0 was added by hand in the Environment Variables tab.

## 6. Things that worked exactly as designed

- Boot chain: postgres healthy → initdb pre-seeds `opn_app` with the real
  password (fresh volume) → core migrates as `opn_migrate` → serves as
  `opn_app` → healthz green after the 60s `start_period`. No manual
  `ALTER ROLE` needed, as the runbook promised.
- `createbucket` one-shot exited `0`; media bucket exists.
- Healthz-gated rollout held routing until core went healthy.
- HSTS + sticky-cookie labels (plain docker-provider middlewares) loaded fine —
  only the file-provider reference (§3) was broken.
- LE issuance via HTTP-01 worked immediately once the router could build
  (grey-cloud/DNS-only Cloudflare record; orange-cloud must stay off because
  coturn needs direct UDP 3478 + relay range to the host IP).

## 7. Secrets hygiene notes (no values in this repo)

- DB and TURN passwords must avoid `@ : / # + =` — they are embedded in
  `postgres://user:pass@host` URLs and coturn's colon-separated `--user=u:p`
  arg. Hex (`openssl rand -hex 24`) is safe. The JWT secret is not URL-embedded,
  so base64 is fine there.
- `OPN_DB_APP_PASSWORD` is consumed by the initdb seed **only on a fresh
  `pgdata` volume**. Changing the secret later does nothing until you manually
  `ALTER ROLE opn_app PASSWORD ...` (runbook §2 note held true).

## 8. Open items

- Re-establish the min-TLS-1.2 floor via Coolify's proxy config, then restore
  the `tls.options` label (§3).
- Update [deploy-rollback.md](runbooks/deploy-rollback.md) §1: the
  hand-created `tls-options.yaml` instruction does not work under Coolify.
- WebRTC path (coturn + `OPN_ICE_SERVERS`) deployed but not yet exercised by a
  real call.
- Cleanup on the host: orphan `tls-options.yaml` in the proxy dynamic dir;
  stray `opn-core-postgres-1`/`opn-core-redis-1` dev-compose containers.
