# opn-panel

Operator admin dashboard (opn-panel-roadmap.md, Sprint P2). Vite + React + TS +
Tailwind. Talks to Core's **private** admin bind (`ADMIN_BIND`, default
`127.0.0.1:9091`) — never the public data plane. Single operator, single admin
password; the JWT lives in browser memory only (no secrets at rest).

## Dev

Core must be running with the admin router enabled — set both `ADMIN_PASSWORD_HASH`
(`opn-core admin hash-password` output) and `ADMIN_JWT_SECRET` in Core's env.
Leave `ADMIN_PANEL_DIR` **unset** in dev; Vite serves the SPA and proxies.

```sh
npm install
cp .env.example .env   # ADMIN_BIND_URL if Core's admin bind isn't 127.0.0.1:9091
npm run dev            # http://localhost:5173, proxies /admin → admin bind
```

## Build (prod)

```sh
npm run build          # → dist/
```

Point Core at the output so the admin bind serves it same-origin (no CORS, no
extra web server):

```sh
ADMIN_PANEL_DIR=/path/to/opn-panel/dist   # in Core's env
```

`/admin/v1/*` stays the API; every other path falls back to the SPA. Reach the
bind over an SSH tunnel or WireGuard — TLS/exposure is the tunnel's job (P3).

## Smoke

```sh
npx playwright install chromium
ADMIN_PASSWORD=<operator-password> PANEL_URL=http://localhost:5173 npm run smoke
```

Drives login → create tenant → key-shown-once → key-absent-after-reload →
rotate → freeze against a running dev stack.
