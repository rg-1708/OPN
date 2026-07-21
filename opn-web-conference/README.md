# opn-web-conference

A deliberately tiny, pure-browser app you **fork into a private repo**: join with a
name, chat in rooms, and place 1:1 A/V calls. Its real job
is to be the **first consumer of the OPN data plane**: it exercises auth, the WS
lifecycle, resume, channels, presence, notify, and calls signaling against a stock
Core. Two things outlive the demo UI: `@opn/client` (the framework-agnostic wire
runtime) and a living proof that *any* UI can be built against the contracts.

> **Status: Sprints W0–W2 built.** What's implemented today is the scaffold,
> `dev-auth` (now with a lobby), the wire client (now with a chat store and a call
> manager), and an app that authenticates, holds a self-healing session, walks
> lobby → rooms → live chat, and places 1:1 voice/video calls. Only packaging
> polish (W3) is not built yet. See the [roadmap](#status--roadmap).

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

## Rooms & live chat

**Rooms** are Core group channels; **live chat** runs over them. `dev-auth` grew a
small lobby — `GET`/`POST /rooms`, `POST /rooms/:id/join`, `GET /rooms/:id/members`
— so the browser can discover, create, and join rooms. The roster is in-memory: a
dev-auth restart loses the lobby list, but the channels themselves survive in Core.
In dev these ride Vite's same-origin proxy (and `deploy/server.mjs` in prod): the
browser also talks to `/rooms` (→ dev-auth) and `/v1` (→ Core REST, for channel
history).

Because Core gates channel membership (`channels.create`/`member_add` are WS-only,
and `member_add` requires the actor to already be a member), the open-join flow
needs an authority that belongs to every room. That's the **lobby bot**: one
`__lobby__` Core WS session `dev-auth` holds, which creates each room and adds every
joiner. It's a test-rig mechanism — a real fork replaces it with the operator's own
lobby/auth.

Chat itself is the new `@opn/client` `ChannelStore` (`createChannelStore`):
optimistic send with `client_uuid` idempotency, seq-ordered reconciliation (a
message never re-orders after its ack), resend-the-same-`client_uuid` on reconnect
(dedupe), HTTP history load on room open, typing indicators, watermark receipts
(auto `mark_delivered`, `mark_read` on focus), and per-member presence dots. A
`notify.event` toast fires for messages in rooms you're a member of but aren't
currently viewing.

## 1:1 calls

Click an online room member's **📞** (voice) or **🎥** (video) button to place a 1:1
call — pure browser, media over **WebRTC**, no plugin. The callee's tab **rings** (a
browser notification *and* an in-app modal) with **accept / decline**; either side
can **hang up**. A call that doesn't connect ends in a distinct state — **busy**,
**declined**, **no answer**, or **ended** — surfaced as UI, not a console log.

**Browser divergence (by design).** In FiveM, call audio rides pma-voice and WebRTC
carries video only (OPN.md §6). The browser has no pma-voice, so the *same* opaque
`calls.signal` relay also carries an audio track — **zero Core change**: the signaling
payload is opaque by design, and this template is the proof that seam holds. The
shared signaling code lives in `@opn/client`
([`packages/client/src/calls.ts`](packages/client/src/calls.ts)) so both templates
stay in sync; only track acquisition differs.

**STUN/TURN.** The browser gets its ICE servers from Core's `calls.state` snapshot
(`ice_servers`), which Core populates from the operator's tenant config — STUN by
default; add a TURN/coturn relay for restrictive NATs. The client just uses whatever
`ice_servers` the snapshot carries (falling back to a public STUN if it's empty). The
`TURN_URL` env is **operator/Core-side coturn config** — it is *not* read by the
browser.

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
7. **Watch it work.** Open **two browser tabs** and join as two different names —
   both reach the green **live** state. Then:
   - **Rooms & chat:** in one tab, **create a room**; **join it** from the other
     tab, and chat. Messages appear **in order** on both sides, and **presence
     dots** and **read receipts** update live.
   - **Self-healing:** kill and restart Core — both tabs go
     **reconnecting → live** without a page reload, and chat **resumes with no
     duplicates**.
   - **Takeover:** open a **third tab as the same name** as an existing tab; the
     first tab surfaces **taken over**.
   - **Calls:** click **📞** or **🎥** next to an online member — their tab **rings**
     (notification + modal); **accept** to connect audio+video, **hang up** to end.
     Two browsers on **separate machines** complete a video-with-audio call through a
     stock Core + STUN.

## Environment

The entire config surface is four variables (see [`.env.example`](.env.example)):

| Variable             | Read by            | Purpose                                                              |
| -------------------- | ------------------ | ------------------------------------------------------------------- |
| `OPN_CORE_URL`       | dev-auth **&** Vite | Core HTTP base (e.g. `http://localhost:8080`). dev-auth mints against it; Vite proxies `/ws` here (http→ws). |
| `OPN_TENANT_API_KEY` | dev-auth **only**  | Tenant API key used to mint sessions. **Server-side only.**          |
| `DEV_AUTH_PORT`      | dev-auth           | dev-auth listen port. Optional, default `8787`.                      |
| `TURN_URL`           | operator / Core    | coturn relay for restrictive NATs. Optional. **Not read by the browser** — the client uses whatever `ice_servers` Core's `calls.state` snapshot carries. |

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
- **The in-memory rooms/lobby and the lobby bot** — placeholders for **your**
  lobby. The roster lives in `dev-auth` memory, and the `__lobby__` bot exists only
  because Core gates channel membership; a real operator's lobby/auth replaces both.
- **The 📞/🎥 call buttons and the phone numbers in the dev-auth member roster** —
  test-rig scaffolding so a call has a target to dial. A real fork drives 1:1 calls
  from the operator's own user directory.
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
| `npm run smoke`     | W0 wire smoke: mint a session and reach `live` against a dockerized Core. |
| `npm run smoke:w1`  | W1 rooms + live-chat smoke: two sessions round-trip over a `ChannelStore` against a dockerized Core. |

## Status / roadmap

- **W0 — done:** scaffold, dev-auth, wire client, app shell that
  authenticates and holds a self-healing session.
- **W1 — done:** rooms & live chat — a `dev-auth` lobby (+ lobby bot) over Core
  group channels, and live chat via the `@opn/client` `ChannelStore`.
- **W2 — done:** 1:1 voice/video calls over WebRTC, signaled through Core
  (`calls.*`) via the `@opn/client` `CallManager`; the browser adds an audio track
  the FiveM template leaves to pma-voice.
- **W3:** packaging polish.
