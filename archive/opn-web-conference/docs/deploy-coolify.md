# Deploy to Coolify

Deploys the web template as a single container behind Coolify's Traefik ingress,
mirroring the Core deploy (`opn-core/docker-compose.prod.yml`). Build source:
[Dockerfile](../Dockerfile) + [docker-compose.deploy.yml](../docker-compose.deploy.yml).

## Topology

```
browser ── https://opn-web.mainframenetwork.com ──▶ Traefik (Coolify ingress, TLS)
                                                        │
                                                        ▼
                                          web container (deploy/server.mjs, :8080)
                                            ├─ GET /*    ▶ static SPA (app/dist)
                                            ├─ POST /join ▶ mint via Core (holds the API key)
                                            └─ WS  /ws    ▶ reverse-proxy to Core's gateway
                                                              │  Host rewritten, Origin preserved
                                                              ▼
                                          wss://opn-core.mainframenetwork.com/ws
```

One Node process serves everything so the browser stays **same-origin** — the
app code is identical to development (no build-time Core URL). Traefik is the
only ingress; the container's server is just the app server, not a second proxy.
The tenant API key lives only in the container's env, never in the browser.

## 0. Prerequisites on Core (once)

Core is already deployed (see `opn-core` deploy-rollback runbook). On the Core side:

1. **Tenant + API key** — run Core's admin CLI (prints the key once):
   ```bash
   opn-core admin create-tenant --name web --new-world web
   # → api key: opn_xxxx…   (this is OPN_TENANT_API_KEY below)
   ```
   Reuse an existing world with `--world <uuid>` instead of `--new-world`.

2. **Allowlist this app's origin** — the WS gateway rejects any browser `Origin`
   not in the tenant's `allowed_origins`. There is **no admin command** for this
   (it is a `text[]` column, default `{}`), so set it with SQL as the DB owner:
   ```sql
   UPDATE tenants
      SET allowed_origins = array_append(allowed_origins, 'https://opn-web.mainframenetwork.com')
    WHERE id = '<tenant-id>';   -- the tenant id printed by create-tenant
   ```
   Without this, tabs connect then immediately close `4401 origin not allowed`.

## 1. DNS

Point `opn-web.mainframenetwork.com` (A/AAAA) at the Coolify host. Coolify
provisions the Let's Encrypt cert on first deploy.

## 2. Coolify app

This is a **single container**, so use the **Dockerfile** build pack — Coolify
generates the Traefik router from the UI, so you write no `traefik.*` labels
(and dodge the `${VAR}`-in-label quirk that bites the compose path).

- **New resource → Dockerfile**, from this repo.
- **Build Pack:** `Dockerfile`. **Base Directory:** the template root (where the
  `Dockerfile` lives).
- **Domain:** set `https://opn-web.mainframenetwork.com` in Coolify's Domains
  field. Coolify builds the router, provisions the Let's Encrypt cert, and
  terminates TLS; WebSocket upgrades pass through automatically. Port `8080`
  (Coolify reads `EXPOSE`).
- **Environment variables:**

  | var | value | notes |
  |---|---|---|
  | `OPN_CORE_URL` | `https://opn-core.mainframenetwork.com` | Core's public base — used to mint and as the `/ws` upstream |
  | `OPN_TENANT_API_KEY` | `opn_…` from step 0 | **secret**, server-side only |

- The container's `HEALTHCHECK` (`/healthz`) gates the rollout.

> **Alternative — Docker Compose build pack.** Use
> [docker-compose.deploy.yml](../docker-compose.deploy.yml) instead if you want
> to pin the Traefik labels yourself — e.g. sticky-cookie WS across multiple
> replicas, or explicit HSTS. Then leave the per-service Domains field **empty**
> (labels define routing), add the env vars by hand, and note that the domain is
> **hardcoded** in the router label because Coolify does not expand `${VAR}`
> inside `traefik.*` labels. For a single replica the Dockerfile pack is simpler.

## 3. Deploy

Deploy from Coolify. It builds the image (multi-stage: Vite build → tiny Node
runtime, no `node_modules`), starts the container, and gates routing on the
`/healthz` healthcheck. Traefik serves `https://opn-web.mainframenetwork.com`.

## 4. Verify

1. Open the domain — the join form loads.
2. Join as a name → the badge goes **Connecting… → Live** (green).
3. Second tab, different name → also **Live**.
4. `docker restart` Core (or redeploy it) → both tabs **Reconnecting… → Live**,
   no reload.
5. Third tab, **same** name as tab 1 → tab 1 flips to **taken over**.

## 5. Update / rollback

- **Update:** push to the tracked branch (or click Redeploy). Coolify rebuilds
  and does a healthz-gated rolling replace.
- **Rollback:** redeploy the previous commit. The web app is stateless — no
  migrations, no volumes — so rollback is just an image swap.

## Gotchas (verified on the Core Coolify deploy)

- **`${VAR}` is not expanded in `traefik.*` labels** — the FQDN is hardcoded.
  It *is* expanded in `environment:`.
- **Entrypoint is `https`**, cert resolver is `letsencrypt` — Coolify's names,
  not stock Traefik's `websecure`.
- **Do not hand-drop `tls-options.yaml`** in `/data/coolify/proxy/dynamic/` — it
  never loads and takes the whole route down. The router uses Traefik's default
  TLS config; raise a min-TLS floor via Coolify's own proxy config.
- **`/ws` hairpins** out to Core's public URL and back through Traefik. Fine for
  a demo. To avoid it, put the web container and Core on a shared Docker network
  and set `OPN_CORE_URL` to Core's internal `http://core:8080`.
- **`TURN_URL` is not used here** — STUN/TURN is Core's `OPN_ICE_SERVERS`,
  delivered to the browser inside `calls.state` (a later sprint exercises it).
