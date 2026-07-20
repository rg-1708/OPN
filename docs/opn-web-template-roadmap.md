# OPN Web Conference Template — Roadmap (draft v0.1)

Companion to [OPN.md](OPN.md) §10 (devices & SDK) and
[opn-core-roadmap.md](opn-core-roadmap.md) (Core build order). Same reading
rules as the Core roadmap: sprints are **scope-bound, not time-bound**; each
sprint has Goal / Depends on / Work items / Test plan / Exit criteria; the
design doc wins on any conflict, and a Core-side change discovered here goes
through a CDR in OPN-CORE.md first.

## What this template is

`opn-web-conference` — a deliberately tiny, pure-browser app users fork into
a private repo: join a lobby with a name, enter a room, chat live, make 1:1
voice/video calls. No FiveM, no game plane. Its real job is **first consumer
of the data plane**: it exercises auth, WS lifecycle, resume, channels,
presence, notify, and calls signaling against a stock Core — the operator's
own end-to-end test rig, and the proof that "the phone is architecturally a
web app" (OPN.md §1.1) is literally true.

Two deliverables fall out of it and outlive it:

1. **`@opn/client`** — the framework-agnostic wire runtime (no React):
   connect, first-frame auth, cmd/ack correlation, `auth.refresh`, heartbeat,
   reconnect with 0–3 s jitter, `sub`/resume with `last_seq`, per-topic event
   dispatch, `message_id` dedupe. This package is a hard dependency of the
   FiveM template's SDK ([opn-fivem-template-roadmap.md](opn-fivem-template-roadmap.md)) —
   building it here, in the simplest possible host, is the point of doing
   the web template first.
2. A living, forkable example of "any UI can be built against `contracts`"
   (OPN.md §10.1 layer 1).

Explicit non-goals: prettiness, mobile support, accounts/registration UX,
persistence beyond what Core stores, N-way conference A/V in v0 (see
Gated items).

## Cross-cutting rules (every sprint)

1. **Contracts from npm only.** Depend on published `@opn/contracts` (exact
   pin); never import from the Rust repo. A template fork must build with
   `npm install` alone. Additive-only semver ([contracts-semver.md](contracts-semver.md))
   is what makes the pin safe.
2. **The tenant API key never reaches the browser.** All minting goes
   through the dev-auth sidecar (Sprint W0). This mirrors the FXServer rule
   (OPN.md §3) — the template must not teach forkers a credential leak.
3. **`@opn/client` stays React-free and style-free.** The app may use
   whatever it likes; the package exports only TS + the contracts types.
4. **Every sprint ends runnable**: `docker compose up` (Core dev stack) +
   `npm run dev` + two browser tabs demonstrating the sprint's exit
   criteria. If the demo needs manual DB fiddling, the sprint isn't done.
5. **TS `strict`, no `any` on the wire path.** Frames are typed by
   `@opn/contracts` end to end.

## Sprint sequence at a glance

| # | Name | Nominal | Depends on | Delivers |
|---|---|---|---|---|
| W0 | Scaffold + dev-auth + wire client | 1–2 w | Core ≥ Sprint 2 | Vite/TS app skeleton, dev-auth sidecar, `@opn/client` core (auth, acks, refresh, heartbeat, reconnect+resume) |
| W1 | Rooms & live chat | 1–2 w | W0 | lobby, room = group channel, send/receive with optimistic UI + seq reconciliation, typing, receipts, presence, resume replay |
| W2 | 1:1 calls (A/V over WebRTC) | 1–2 w | W1 | calls FSM UI, `calls.signal` WebRTC (audio **and** video in browser), STUN + optional TURN |
| W3 | Template packaging | 1 w | W2 | fork guide, config surface, CI, smoke test against dockerized Core |

Total nominal: ~5 weeks. W-numbering is deliberate — these are not Core
sprints; Core version prerequisites are stated per sprint.

## Sprint W0 — Scaffold, dev-auth, wire client

**Goal**: a blank page that provably holds an authenticated, self-healing WS
session to a stock Core.

**Depends on**: Core Sprints 0–2 (health, identity/auth, WS gateway).

### Work items

1. **Repo scaffold**: Vite + vanilla TS (React explicitly not required at
   this layer; the template stays framework-thin so forkers see the wire,
   not our component taste). Modern browser APIs are fair game here — no
   Chromium 103 floor in a real browser; the 103 constraint binds only the
   FiveM template. Styling: Tailwind **v4** (decided) — real browsers only,
   so its modern-CSS output is fine here. Consequence, accepted: the two
   templates pin different tailwind majors (v3 in FiveM is a hard CEF-103
   constraint, not a preference); styles never cross the template boundary,
   and `@opn/client` is style-free, so nothing shared drifts. Workspace: `packages/client` (`@opn/client`),
   `app/` (the conference UI). `@opn/contracts` pinned from npm.
2. **Dev-auth sidecar** (`dev-auth/`, single file, ~100 lines, node): holds
   `OPN_TENANT_API_KEY` + Core base URL from env. One endpoint:
   `POST /join { name }` → resolves/creates the character for
   `framework_ref = name` and mints a session via
   `POST /v1/tenants/self/sessions`, returns `{ token, session_id,
   character }` to the browser. It plays the FXServer's role in the auth
   chain (OPN.md §3) and nothing more. *Open question to close in this
   sprint (Core-side): whether session mint upserts an unknown
   `framework_ref` or requires a separate character-create call — whichever
   it is, the sidecar wraps it; if Core lacks a path entirely, that is a
   CDR conversation before any template code is written.*
3. **`@opn/client` core**:
   - `connect(url, token)`: WS open, `auth` first frame within the 3 s
     window, surface close codes (`4400/4401/4408/4409`) as typed errors.
   - Cmd/ack correlation by frame id (promise per in-flight cmd), typed by
     the contracts `Cmd`/ack types.
   - `auth.refresh` timer (re-mint JWT well before the 10 min exp; store
     the fresh token for reconnect + HTTP).
   - Reconnect with 0–3 s jitter (OPN.md §7), resubscribe with `last_seq`
     per replayable topic; deliver replayed events through the same
     dispatch path so the app can't tell replay from live.
   - Event dispatch: `on(topic, handler)`; `message_id` dedupe ring per
     channel topic (at-least-once to UI, exactly-once render — OPN-CORE.md §8).
4. **App shell v0**: name form → dev-auth `/join` → connect → connection
   state indicator (connecting / live / reconnecting), visible session
   identity. Nothing else.

### Test plan

- `@opn/client` unit tests against a scripted mock WS server: auth timeout,
  bad-first-frame close, ack correlation under interleaving, refresh, dedupe.
- One integration script (`npm run smoke`) against the real dockerized Core:
  join, connect, survive a forced Core restart (container kill), reconnect,
  session still valid or cleanly re-minted.

### Exit criteria

- [ ] Two tabs join as different names; both show `live`.
- [ ] `docker restart` of Core: both tabs show `reconnecting` then `live`
      without a page reload.
- [ ] Second tab with the same name/session takes over; first tab surfaces
      the `4408` takeover state instead of silently dying.
- [ ] API key greppable nowhere in browser-served assets.

## Sprint W1 — Rooms & live chat

**Goal**: two strangers pick the same room and talk, with the full delivery
semantics (optimistic send, seq order, receipts, resume) visibly correct.

**Depends on**: W0; Core Sprints 3–4 (channels complete, history HTTP,
cursor idiom).

### Work items

1. **Rooms as group channels.** Dev-auth grows a room roster:
   `POST /rooms { name }` → `channels.create` (kind `group`, creator = the
   requesting character); `POST /rooms/:id/join` → `channels.member_add`.
   Room list lives in dev-auth memory (it is a test rig, not a product —
   restart loses the lobby list, channels themselves survive in Core).
2. **Chat pane** over `useChannel`-shaped client helpers (plain TS store,
   not React hooks — the hook layer belongs to the FiveM SDK):
   - `channels.send` with `client_uuid` idempotency; optimistic append,
     reconcile position by acked `seq`; retry-on-reconnect resends the same
     `client_uuid` (dedupe proof).
   - History: HTTP cursor fetch on room open, then live events; the >500
     gap fallback (client told to cold-load) handled, not special-cased.
   - Typing (throttled), watermark receipts (`mark_delivered` automatic,
     `mark_read` on focus), reactions optional — include only if free.
3. **Presence**: snapshot-on-sub + live events → member list with
   online/offline dots.
4. **Notify surface**: minimal toast on `notify.event` for messages in
   rooms you're a member of but not viewing (proves ring/alert/silent
   classes reach a real UI).

### Test plan

- Playwright (or equivalent) two-context test: send order, optimistic
  reconciliation, typing, read receipts crossing tabs.
- Chaos-lite script: kill Core mid-send → sender reconnects, resends,
  exactly one message renders in both tabs (the delivery guarantee, seen
  from the client side).

### Exit criteria

- [ ] Two tabs, same room: messages appear in seq order both sides;
      offline tab catches up via resume on reconnect, no duplicates.
- [ ] Sender sees the message the instant they hit enter, and it never
      re-orders after ack.
- [ ] Read receipts and presence dots update live.

## Sprint W2 — 1:1 calls, audio+video over WebRTC

**Goal**: click a member, ring them, talk — pure browser.

**Depends on**: W1; Core Sprint 6 (calls + signaling relay).

### Work items

1. **Call UI**: call button per online member (`calls.start
   { callee_number, video }` — numbers come from `identity.me`/directory),
   incoming ring via `notify` class `ring` (browser notification + in-app
   modal), accept/decline/hangup driving the Core FSM; render from
   `calls.state` snapshots only (no client-side FSM guessing).
2. **WebRTC over `calls.signal`**: offer/answer/ICE relayed opaquely.
   **Browser divergence, document it in the template README**: in FiveM,
   audio stays in pma-voice and WebRTC carries video only (OPN.md §6); in
   the browser there is no pma-voice, so the same opaque signaling carries
   an audio track too. Zero Core change — the payload is opaque by design;
   this template is the proof that seam holds.
3. **NAT path**: STUN default; `TURN_URL` env passthrough for the coturn
   relay when present. Capped video (~480p) matching the FiveM budget.
4. **Busy/decline/timeout paths surfaced**: `conflict`-busy, 60 s ring reap —
   real UI states, not console logs.

### Test plan

- Two-context Playwright with fake media devices: full ring→accept→media
  flow, decline, hangup, caller-disappears (tab close) → janitor reap
  observed within 60 s.
- Signal-relay unit tests: only active participants can signal (a third
  session's `calls.signal` acks `forbidden`).

### Exit criteria

- [ ] Two browsers on separate machines complete a video call with audio
      through a stock Core + STUN.
- [ ] Decline, busy, hangup, and ring-timeout all render distinct correct
      states in both tabs.

## Sprint W3 — Template packaging

**Goal**: a stranger forks the repo and is on a call against their own Core
within 15 minutes.

**Depends on**: W2.

### Work items

1. **Config surface**: exactly three env vars (`OPN_CORE_URL`,
   `OPN_TENANT_API_KEY` (dev-auth only), `TURN_URL` optional). Everything
   else is convention.
2. **Fork guide** (README): the 15-minute path — clone, compose up Core,
   set key, `npm run dev`; plus "what to rip out" notes (dev-auth is a
   placeholder for *your* auth; rooms-in-memory is a placeholder for *your*
   lobby).
3. **CI**: typecheck, unit tests, and the W0 smoke against a dockerized
   Core on every push. Contracts pin bump = PR with the smoke as the gate.
4. **`@opn/client` extraction check**: the package builds standalone and is
   consumed by path/workspace dep only for now; npm publish is deferred
   until the FiveM template consumes it (second consumer proves the API,
   same logic as OPN.md §10.1's "revisit when a second real consumer
   demands more").

### Exit criteria

- [ ] Fresh-clone-to-first-call runbook executed once, timed, by someone
      (or a clean VM) that isn't the author.
- [ ] CI green including the real-Core smoke.

## Gated items (named, not scheduled)

- **N-way conference A/V.** Core calls are strictly 1:1: `calls.start`
  takes one callee and the busy-check (any participant row in a non-ended
  session → `conflict`) forbids the pairwise-mesh workaround. A true
  conference needs a Core decision first — a `calls.join`/room-call kind
  (contracts-minor) or a dedicated signaling topic — i.e. a CDR in
  OPN-CORE.md, not template code. Until then "conference" in this template
  means: group chat room + 1:1 calls between members. This is an honest
  v0 and already exercises every primitive the FiveM phone needs.
- **Media** (avatars, image messages in rooms): straightforward consumer of
  Core Sprint 5, adds nothing the FiveM template won't test better
  (gallery/camera). Add only if a Core media regression needs a web repro.
- **`@opn/client` npm publish**: gated on the FiveM template (second
  consumer), see W3.

## Risks worth naming

- **Session mint for non-FiveM clients** may not exist in the shape the
  dev-auth sidecar wants (upsert-by-`framework_ref`). Surface early in W0;
  it is one CDR + at most a contracts-minor, but it blocks everything.
- **Browser WebRTC divergence** could silently drift the template away from
  the FiveM video path. Parry: the signaling code lives in `@opn/client`
  helpers shared by both templates; only track acquisition differs.
- **Scope creep toward a product.** This is a test rig with a UI. The
  ladder rule: any feature not exercising a Core primitive is out.
