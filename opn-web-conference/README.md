# opn-web-conference

A deliberately tiny, pure-browser app you **fork into a private repo**: join with a
name, and — in later sprints — chat in rooms and place 1:1 A/V calls. Its real job
is to be the **first consumer of the OPN data plane**: it exercises auth, the WS
lifecycle, resume, channels, presence, notify, and calls signaling against a stock
Core. Two things outlive the demo UI: `@opn/client` (the framework-agnostic wire
runtime) and a living proof that *any* UI can be built against the contracts.

> **Status: Sprint W0 only.** What's implemented today is the scaffold, `dev-auth`,
> the wire client, and an app shell that authenticates and holds a self-healing
> session. Chat/rooms (W1), calls (W2), and packaging polish (W3) are not built
> yet. See the [roadmap](#status--roadmap).

## Architecture

The browser never sees the tenant key or the Core URL. Vite's dev server proxies
same-origin, so the page only ever talks to its own origin:

```
                       ┌──────────────────────────────────────────┐
  browser              │           Vite dev server (proxy)         │
  ┌───────────┐        │                                           │
  │ app shell │  /join ─┼──────────────►  dev-auth  ─── Bearer key ─┼──► Core  POST /v1/tenants/self/sessions
  │ @opn/     │  (HTTP) │              (holds OPN_TENANT_API_KEY)   │        (mints a session JWT)
  │  client   │         │                                           │
  │           │   /ws  ─┼──────────────────────────────────────────┼──► Core  WS  (scheme http→ws)
  └───────────┘  (WS)   └──────────────────────────────────────────┘
```

- The **tenant key lives only in `dev-auth`** (server side). It is never shipped to
  the browser.
- **Durable deliverables** (you keep these after forking): `@opn/client` — the wire
  runtime (connect, auth, cmd/ack, refresh, reconnect+resume, dedupe) — and
  `@opn/contracts` — the TS wire types.

## The 15-minute path

1. **Clone** this repo (into your private fork).
2. **Start a stock Core** — in your `opn-core` dev stack:
   ```bash
   docker compose up
   ```
3. **Configure** — copy the example env and fill in two values:
   ```bash
   cp .env.example .env
   # then set:
   #   OPN_CORE_URL=http://localhost:8080
   #   OPN_TENANT_API_KEY=<your tenant API key>
   ```
4. **Install**:
   ```bash
   npm install
   ```
5. **Run dev-auth** (terminal 1 — reads `OPN_CORE_URL` + `OPN_TENANT_API_KEY`):
   ```bash
   npm run dev:auth
   ```
6. **Run the app** (terminal 2 — proxies `/join` to dev-auth and `/ws` to Core):
   ```bash
   npm run dev
   ```
7. **Watch it work.** Open **two browser tabs**, join as two different names, and
   watch both reach the green **live** state.
   - **Self-healing:** kill and restart Core — both tabs go
     **reconnecting → live** without a page reload.
   - **Takeover:** open a **third tab as the same name** as an existing tab; the
     first tab surfaces **taken over**.

## Environment

The entire config surface is four variables (see [`.env.example`](.env.example)):

| Variable             | Read by            | Purpose                                                              |
| -------------------- | ------------------ | ------------------------------------------------------------------- |
| `OPN_CORE_URL`       | dev-auth **&** Vite | Core HTTP base (e.g. `http://localhost:8080`). dev-auth mints against it; Vite proxies `/ws` here (http→ws). |
| `OPN_TENANT_API_KEY` | dev-auth **only**  | Tenant API key used to mint sessions. **Server-side only.**          |
| `DEV_AUTH_PORT`      | dev-auth           | dev-auth listen port. Optional, default `8787`.                      |
| `TURN_URL`           | (reserved)         | TURN relay for WebRTC calls. Optional; used by a later sprint.       |

> **Hard rule:** `OPN_TENANT_API_KEY` is read by `dev-auth` and **must never reach
> the browser**. dev-auth exists precisely so the key stays server-side — mirroring
> the FXServer's role (OPN.md §3). Don't teach forkers a credential leak.

### The auth chain

`browser POST /join { name }` → `dev-auth` calls Core
`POST /v1/tenants/self/sessions` with `Authorization: Bearer $OPN_TENANT_API_KEY`
and body `{ framework_ref: name }` → Core upserts a character by `framework_ref` and
returns a session JWT → dev-auth returns `{ token, session_id, character }` to the
browser → `@opn/client` opens the WS, sends `auth` as the first frame, and holds the
session (auto-refresh, reconnect+resume).

## What to rip out when you fork

- **`dev-auth/`** — a placeholder for **your** auth. It mirrors the FXServer's
  session-minting role; swap it for however your app authenticates users and mints
  Core sessions.
- **The in-memory rooms/lobby** (arriving in W1) — a placeholder for **your** lobby.
- **Keep `@opn/client` and `@opn/contracts`.** Those are the durable pieces.

## Contracts note

`@opn/contracts` is currently **vendored** — generated TS types committed under
[`packages/contracts`](packages/contracts/README.md) — as a stand-in until the npm
package publishes. A fork will later just depend on the published `@opn/contracts`
and delete the vendored copy. Details: [`packages/contracts/README.md`](packages/contracts/README.md).

## Commands

| Command             | What it does                                                             |
| ------------------- | ----------------------------------------------------------------------- |
| `npm install`       | Install workspaces.                                                      |
| `npm run dev:auth`  | Start dev-auth (needs `OPN_CORE_URL` + `OPN_TENANT_API_KEY`).            |
| `npm run dev`       | Start the Vite app (proxies to dev-auth + Core).                         |
| `npm test`          | Run the `@opn/client` unit tests (`node --test`, zero deps).            |
| `npm run typecheck` | `tsc` over the client + app projects.                                                                |

## Status / roadmap

- **W0 — done (this):** scaffold, dev-auth, wire client, app shell that
  authenticates and holds a self-healing session.
- **W1:** rooms & chat.
- **W2:** 1:1 A/V calls.
- **W3:** packaging polish.
