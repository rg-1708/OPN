# OPN — System Design (v0.1 brainstorm)

**OPN** (as in *open source*) — an in-game device platform for FiveM: a phone
first, PCs and other devices later. Not "a phone script" — a **device OS shell
+ a reusable realtime backend (Core)** that apps compose. Everything except
Core internals is open source (§10.1): the project is a framework other
servers reskin and extend, not a product they theme.

Decisions locked so far:

| Decision | Choice |
|---|---|
| Scope | Our own server(s) first; design product seams, implement only our path |
| Backend stack | Performance-first → **Rust Core** (see §8 for the honest trade-off) |
| Transport | **Direct WSS** NUI ↔ Core; game server only mints tokens |
| Topology | Core is **multi-tenant** (serves N game servers) from day one |

---

## 1. The two load-bearing ideas

### 1.1 Two planes instead of one spine

lb-phone (and every other FiveM phone) funnels all app data through
`net event → server Lua → oxmysql`. That means chat history, feeds and photos
compete with game traffic for the FXServer main thread and msgpack-serialized
net events. That is the performance ceiling we refuse.

- **Game plane** — client Lua ↔ server Lua, small RPC surface. Only what
  *requires* game state: identity resolution, money/items (framework bridge),
  voice targets, animations/props/camera, proximity, keybinds.
- **Data plane** — NUI (CEF) ↔ Core over one multiplexed WSS connection +
  HTTPS for bulk reads/media. All app data: messages, feeds, typing, presence,
  notifications, media. **Zero bytes of app data cross FiveM netcode.**

The game server's role in the data plane is exactly one thing: after a player
selects a character, it calls Core (server-to-server, API key) to mint a
short-lived session token and hands it to the NUI. From then on the phone is,
architecturally, a web app that happens to be rendered inside GTA.

### 1.2 Primitives, not apps

The Core exposes ~8 domain primitives. Apps are thin UI compositions over
them — a new app is UI + a manifest, not a new backend.

| Primitive | Responsibility | Powers |
|---|---|---|
| `identity` | devices, numbers, per-app accounts, sessions | everything |
| `channels` | conversations: members, messages, receipts, typing, reactions | Messages, DarkChat, group chat, Tinder matches, Mail |
| `feed` | posts, follows, likes, comments, hashtags | social apps (post-MVP) |
| `calls` | call sessions + signaling (voice stays in-game, video via WebRTC, §6) | Dialer |
| `ledger` | accounts, transfers, holds, tx history | Wallet, crypto, marketplace checkout |
| `media` | presigned upload/download, thumbnails, retention | Camera, all apps |
| `directory` | contacts, blocklists, public listings | Contacts, YellowPages |
| `notify` | routing: online push / offline inbox / HUD toast | everything |

Examples of composition:
- **Messages** = `channels(kind=sms)` + `directory` + `media` + `notify`
- **Guild-style chat app** (post-MVP) = `channels` + `calls` + `media` + `notify` — *zero new backend*
- **Marketplace** = own listings table + `ledger` (escrowed hold) + `channels(kind=dm)` for buyer↔seller

Rule of thumb: an app may own *app-specific* tables (listings, tinder profiles),
but anything that is "people exchanging messages/money/media/attention" MUST go
through a primitive. That's the reuse contract.

---

## 2. Topology

```
┌─ GAME CLIENT ────────────────────────────────┐
│  NUI (CEF)  ── one SPA, all devices          │
│  ┌─────────────────────────────────────────┐ │
│  │ Shell (phone / later: pc, tablet)       │ │
│  │ App runtime + SDK  │ apps as lazy chunks│ │
│  └───────┬──────────────────────┬──────────┘ │
│          │ NUI msgs             │ WSS+HTTPS  │──────────┐
│  client Lua runtime (thin):     │            │          │
│  prop/anim/camera/keybinds,     │            │          │
│  voice submix, HUD toasts       │            │          │
└──────────┼──────────────────────┼────────────┘          │
           │ 2-event RPC (game plane)                     │ data plane
┌──────────┴───────────────┐                              │ (direct, TLS)
│  FXSERVER  (per tenant)  │                              │
│  gateway resource (Lua): │   server-to-server           │
│  framework bridge, money │   HTTPS + API key            │
│  items, voice ctl, token ├──────────────┐               │
│  minting, admin cmds     │              │               │
└──────────────────────────┘              ▼               ▼
                              ┌─ COOLIFY ──────────────────────────────┐
                              │  opn-core (Rust, one binary, N repl.)  │
                              │  ws-gateway │ http api │ primitives    │
                              │  ┌────────┐ ┌──────────┐ ┌──────────┐  │
                              │  │Postgres│ │  Redis   │ │  MinIO   │  │
                              │  │ (sqlx) │ │pubsub/   │ │presigned │  │
                              │  │        │ │presence  │ │ media    │  │
                              │  └────────┘ └──────────┘ └──────────┘  │
                              │  Traefik: TLS, sticky WS, health       │
                              └────────────────────────────────────────┘
```

Repos/deployables:

1. **opn-core** — Rust modular monolith. WS gateway + HTTP API + all
   primitives in one binary. Scales by replicas behind Traefik (sticky WS,
   Redis pub/sub for cross-replica fan-out). Split into services only when a
   measured bottleneck demands it — at FiveM scale it won't.
2. **opn-ui** — TS monorepo: `shell-phone` (later `shell-pc`), `sdk`,
   `apps/*`, `contracts` (generated from Rust, §4).
3. **opn-fivem** — the FiveM resource: client runtime Lua, gateway Lua,
   framework bridges (`custom/`-style, one impl loads per config).

---

## 3. Identity & multi-tenancy

- `tenant` = one FXServer (API key, allowed origins, config).
- `world` = data scope. Every domain row is keyed by `world_id`.
  Tenant → world is a mapping: N tenants can share one world (dev+prod against
  staging worlds, or clustered RP servers sharing one phone network later).
  This one indirection is what makes "multi-server" free instead of a migration.
- `character` = (world_id, framework char id). Owns devices and **the phone
  number** (unique per world, assigned on first phone; decided §14.1).
- `device` = a phone (or later a PC). Pure hardware — a UI endpoint for
  notify routing and settings; carries no number. All of a character's
  devices share the character's number.
- `app account` = per-app identity on top of a character (social-app handles,
  usernames). One character can have several; login switching is an
  `identity` feature, not per-app code.

Auth chain: player picks character → gateway resource resolves char via
framework bridge → `POST /v1/tenants/self/sessions {char_id, device_id}` with
tenant API key → Core returns a short-lived JWT (~10 min, auto-refreshed over
the WS) → NUI opens `wss://core.example.com/ws` and sends the token as the
first frame (never in the URL — query strings land in proxy logs; see
OPN-CORE.md §4.1). NUI never holds long-lived secrets; the tenant key never
leaves the FXServer.

---

## 4. Contracts — single source of truth

The reusable-API goal lives or dies here. **Types are defined once, in Rust**,
and exported to TypeScript at build time (`ts-rs`/`specta`):

- Rust structs/enums with `serde` → the wire format.
- Generated `.d.ts` published as `@opn/contracts` → shell, SDK and every
  app import the exact same types. A tagged Rust enum becomes a discriminated
  union in TS — exhaustive `switch` in app code, compiler-checked both ends.
- The WS envelope and every primitive's commands/events are part of this
  contract. No hand-written duplication, no OpenAPI drift.

WS protocol (one connection per NUI session, multiplexed):

```
Client→Server: { id, cmd: "channels.send", payload: { channel_id, client_uuid, body } }
Server→Client: { reply_to: id, ok: true, payload: { message_id, seq } }        // ack
Server→Client: { evt: "channels.message", topic: "ch:123", payload: {...} }   // push
Subscriptions: cmd "sub" / "unsub" per topic; shell subscribes to notify:*,
               apps subscribe to their topics on mount, unsub on unmount.
```

JSON first (debuggable); envelope is transport-agnostic so we can flip to
msgpack/CBOR later behind the same types if profiling ever cares.

---

## 5. Delivery semantics (from the messaging reference, kept)

- **Persist-then-ack.** Insert to Postgres, then ack `SENT`. No ack from
  memory, ever.
- **Ordering**: per-channel monotonic `seq` assigned at insert. Clients render
  by seq, not arrival.
- **Idempotency**: client sends a UUID per message; unique index on
  `(channel_id, client_uuid)` makes retries safe.
- **Resume**: client tracks `last_seq` per subscribed topic; on reconnect,
  `sub` carries it and Core replays the gap from Postgres/Redis. WS drops
  (map teleports, CEF hiccups) are invisible to the user.
- **Receipts**: delivered/read as lightweight events on the same topic.
- **Recipient offline** (player not on the server): row is already persisted;
  `notify` writes an inbox entry surfaced at next login. In-game "offline"
  (phone closed) is NOT offline — the shell keeps the WS alive and renders
  HUD toasts via a small NUI→client-Lua hook.
- **Airplane mode / no service zones**: client-side gate in the shell — events
  queue in `notify` inbox and replay when the gate opens. Purely cosmetic to
  the Core.

---

## 6. Calls — signaling in Core, voice in game, video via WebRTC

Voice audio never touches Coolify. `calls` manages session state
(ring, accept, decline, hold, participants) over the data plane; on `accept`,
Core emits events the gateway resources consume, and each FXServer's gateway
resource sets voice targets via the voice bridge (pma-voice first: call
channels + submix for phone EQ). This keeps latency native and cost zero.

**Video calls** (MVP): video is the one flow in-game voice can't carry, so it
rides WebRTC between the two NUIs — **video track only; audio stays in
pma-voice** (keeps proximity/radio machinery consistent; minor lip-sync
offset accepted). Core's role is signaling only: offer/answer/ICE relayed as
`calls.signal` events over the existing call topic — video bytes never touch
Core. Frame source: on accept, client Lua points a face camera at the
character (same game-plane camera surface the Camera app uses); NUI captures
it via `useGameRender` and `canvas.captureStream()` (capped ~480p). NAT
traversal: STUN + a coturn TURN relay on Coolify as fallback — coturn idles
at ~zero (passes the ADR-3 idle-cost rule) and only carries relayed video
during calls where P2P fails.

Future PC-outside-game client (§10) reuses the same signaling; only the
audio transport would swap there.

---

## 7. Performance strategy (the actual budget)

Where phone latency really comes from, in order: geography (RTT), DB access
patterns, fan-out design, GC pauses, serialization. The design attacks each:

**FXServer (the scarcest resource on any RP server)**
- 0.00ms idle: no per-frame loops while phone closed; statebags for open/prop
  state; no polling anywhere.
- Game-plane RPC surface is tiny (~a dozen ops) and rate-limited per source.
- No DB access from Lua at all. The game box runs no query load for the phone.

**Core (Rust)**
- Targets: command processing p99 < 5 ms; send→ack p50 dominated by RTT only;
  10k msg/s on one modest node without tuning (FiveM peak is ~10 msg/s at
  500 DAU — headroom is the point, plus it's the product story later).
- sqlx prepared statements, pipelined writes; `messages` partitioned monthly
  per world → cheap retention (`DROP PARTITION`), hot set stays in RAM.
- Redis: presence (TTL keys), pub/sub per topic for cross-replica fan-out,
  offline inbox streams. Single Core replica doesn't even need the pub/sub hop.
- Backpressure: per-connection send queues with drop-oldest for ephemeral
  events (typing, presence), never for messages.

**NUI (CEF render budget — where phones actually lag)**

Backend p99 is invisible; CEF compositing on top of GTA's frame budget is
what players feel. Rules:
- One `ui_page` SPA; each app is a lazy route chunk — the shell boots fast,
  apps load on first open, then are warm.
- Per-app stores (zustand-style), no god-context: activity in one app must
  not re-render another.
- Virtualized lists for every scrollable surface; thumbnails from `media`
  (never full-size in lists); optimistic UI on sends (reconciled by ack seq).
- `backdrop-filter` doesn't exist in FiveM CEF at all (see
  `restrictions.md`) — pre-baked blur or opacity is the only option (the
  reason lb-phone ships `fixBlur`). Animations are transform/opacity only;
  capped shadow layers; `content-visibility` on offscreen app surfaces.
- **Build target: Chromium 103.** FiveM CEF also lacks `:has()`, container
  queries, CSS nesting, `dvh`/`svh`, `oklch()`/`color-mix()`, subgrid,
  `text-wrap: balance`, anchor positioning (full list: `restrictions.md`).
  Enforced in tooling, not memory: `browserslist: chrome 103` so
  Vite/Lightning CSS transpiles or rejects — reskins inherit the guard.
- Game render (camera viewfinder) runs only while its component is mounted,
  destroyed on unmount. lb-compat iframes mount on app open, are killed on
  close — each idle iframe is ~10–30 MB of CEF memory.
- Reconnect jitter: on WS drop the shell retries with 0–3 s random delay —
  kills the thundering herd when an FXServer restart reconnects every player
  at once (token mint burst + gap replays).

---

### 7.1 Sizing baseline & footprint budget

Design point: **~1–2k registered users, a few hundred concurrent** (bounded by
FXServer slots). Hard ceiling we must be able to *grow into* without redesign:
100k registered. We size for the former and only keep seams for the latter.

Capacity math at the design point (2k registered, ~800 DAU, 300 concurrent):
- Messages: 800 × 40/day = 32k/day ≈ **0.4 msg/s avg, ~30 msg/s peak** — noise.
- WS connections: ~300 concurrent (one per online player) — noise.
- Storage: tens of MB/day text; media dominates and lives in MinIO with
  lifecycle purge, not in Postgres.

Footprint budget — allowance: **up to 6–8 GB RAM, 20% CPU at peak, disk for
cache/retention** (relaxed from the original <2 GB target; decided 2026-07):
- Spend the headroom where it measurably helps: **Postgres gets 2–4 GB**
  (`shared_buffers` 1–2 GB, rest OS page cache) so the full message/feed hot
  set is RAM-resident — every query an index walk over cached pages. This
  also lets message retention grow (60–90 days) at zero perf cost; disk
  bounds it, not speed.
- Everything else stays lean by design: Rust core ~30–60 MB / ~0% CPU idle;
  Redis ~50 MB; MinIO ~200 MB. One Core replica; Redis pub/sub hop not
  needed until a second replica exists.
- Deliberately NOT spent on: in-process caches in Core (RAM-resident PG read
  is ~100 µs — a cache layer adds invalidation bugs, saves nothing felt),
  precomputed timelines, server-side media cache (content-addressed
  immutable objects are cached client-side; each client fetches once, ever).
  Player-felt latency lives in RTT and CEF rendering, not server resources.
- Single-binary monolith rationale unchanged: no per-service runtimes,
  sidecars, or mesh idling away CPU.

Scale path (only when measured, in order): bigger node → 2nd Core replica
(sticky WS + Redis pub/sub, already designed in) → move Postgres/Redis to a
2nd Coolify server → read replicas / partition-per-world sharding. That path
covers 100k registered without touching app code or contracts.

### 7.2 Media pipeline (decided)

Media is the only flow that moves megabytes; everything else is kilobytes.
Principle: **the node never does image CPU, and every byte is fetched once.**

- **Capture**: FiveM CEF exposes the game frame as a GPU texture to NUI WebGL
  (the `screenshot-basic` mechanism). The SDK wraps it once (`useGameRender`:
  WebGL quad → canvas, `takePhoto()`, `MediaRecorder` for video); our Camera
  app and the lb-compat shim (§10.2) share this module. Viewfinder renders
  only while mounted; recording is the accepted FPS cost.
- **Client-side thumbnails**: the uploader's canvas already holds the full
  frame — one `drawImage` to a small canvas + `toBlob` makes the thumb on the
  player's machine, distributed across 300 clients instead of centralized on
  the game host. Video poster frame: same trick at record start. Core does
  zero decode/resize; no thumbnail sidecar exists. A forged thumb is a
  cosmetic-only risk (wrong preview), authz unaffected. Imported/external
  media may stay thumb-less or thumb lazily.
- **Flow**: `media.request_upload(kind, bytes, mime)` → two presigned PUTs
  (original + thumb) with caps baked in (photo ≤ 2 MB, video ≤ 25 MB, audio
  ≤ 1 MB voice messages, thumb ≤ 40 KB, MIME-checked at presign, enforced by
  MinIO policy) →
  `media.commit(media_id)` → row live. Core is never in the byte path.
- **Egress**: content-addressed keys + immutable `Cache-Control` — each CEF
  client fetches an object once, ever. Lists render thumbs only; full-size on
  tap. Popular-message burst math: 100 KB thumb × 300 players = 30 MB once,
  then cached. If egress ever measurably hurts, a CDN/proxy-cache slots in
  front of MinIO GETs — URL indirection via `media` already allows it; not
  built now.
- **Retention**: MinIO lifecycle purge (30–60 days), gallery favourites
  exempt via object tag. Postgres stores keys only, never blobs.

## 8. Stack choice

**DECIDED: Rust (axum + tokio + sqlx + redis-rs), modular monolith.** (See
ADR-1 for the numbers behind it.)

- At this scale Go or even Node would *also* be fast enough; the honest gains
  of Rust are: no GC pauses on the WS fan-out path, lowest memory per
  connection, one static binary (trivial Coolify deploy), and a credible
  "Rust core" story if this becomes a product.
- The build-cost tax is real but contained: the Core is a bounded set of
  primitives, not sprawling microservices. CRUD in axum+sqlx is routine.
- The escape hatch is architectural, not aspirational: everything speaks the
  generated contract (§4). If Rust iteration speed ever hurts, a Go port of a
  primitive is mechanical.
- UI stays React+TS. Lua stays thin. So Rust surface area = Core only.

## 9. Security model

- NUI is untrusted (players can inspect CEF). Every command is authorized
  server-side in Core against the JWT's (world, character, device) — never
  against client-supplied ids.
- Tenant API keys only on FXServer; JWTs short-lived; token refresh over WS.
- Anything touching money/items/game state goes through the game plane where
  the gateway resource re-validates against the framework — the Core `ledger`
  is authoritative for phone-money, the framework bridge is authoritative for
  bank/cash sync points.
- Rate limits per (tenant, character) in Core; per-source on game-plane RPC.
- Media: presigned PUT to MinIO with size/MIME caps; URL host allowlist in the
  shell (lb-phone got this right).
- Traefik: TLS via Let's Encrypt, CORS locked to expected origins, Core also
  validates `Origin`.

## 10. Devices & the app SDK

- **Manifest per app**: id, icons, targets (`phone`, `pc`), required
  primitives, notification types, background permissions. The shell composes
  the launcher from manifests — installing an app is data, not code changes.
- **Shells share the runtime**: `shell-phone` (springboard, status bar,
  gesture nav) and later `shell-pc` (window manager, taskbar) are different
  chrome around the same app modules. An app that only uses SDK layout
  primitives runs on both; apps can ship per-device layouts when they care.
- **SDK surface**: contracts client (typed commands/subscriptions), navigation,
  notifications, media picker, contact picker, theming tokens, `useGame()` for
  the rare game-plane call. First-party apps are in-repo modules; a
  third-party iframe/postMessage SDK is a product-phase feature — the manifest
  and SDK boundary are designed so it bolts on without rework (first consumer:
  lb-compat, §10.2).

### 10.1 Open-source framework & reskinning

Goal: another server adopts OPN and ships its own visual identity without
touching logic. Three layers, strictly ordered:

1. **`contracts`** — generated from Rust (§4). The public framework surface;
   any UI can be built against it.
2. **`sdk` headless layer** — app *logic* as headless hooks/stores
   (`useChannel(id)`, `useFeed(handle)`, `useCall()`): subscriptions,
   optimistic sends, seq reconciliation, resume (§5) — zero JSX. A reskin
   rewrites components over the same hooks; the hard correctness 80% stays
   shared. The `sdk` public surface depends only on React + the contracts
   client — no state/styling library leaks, or every reskin inherits our stack.
3. **`shell-*` + `apps/*` components** — the reference implementation.
   Reskin path: design tokens (CSS variables: colors, radius, fonts,
   wallpaper; icons via manifest) cover ~90% of visual identity; a full
   re-theme forks/replaces components, headless layer untouched.

No runtime plugin/component-injection system for skinning — token theming +
fork-friendly repo structure covers it; revisit only when a second real
consumer demands more.

Licensing split:
- **Open (MIT)**: `contracts`, `sdk`, `shell-*`, `apps/*`, `opn-fivem` — the
  framework.
- **Closed (or BSL)**: `opn-core` internals. Consumers never need Core source;
  they need the published contract types. Consequence: `@opn/contracts` is a
  public API promise — semver, additive-only within a major.

### 10.2 lb-phone app compatibility (lb-compat)

Third-party lb-phone custom apps run on OPN unmodified. Feasible because lb
apps are self-contained FiveM resources: own iframe UI (`cfx-nui-<res>`
origin), own Lua + DB reached via `fetchNui` to their own origin. They never
touch our data plane — compat is a **shell-services shim, not data
translation**.

Shape (separate optional `lb-compat` package, SDK core stays clean):

1. `AddCustomApp` export in `opn-fivem` — maps the lb manifest (identifier,
   name, ui url, icon, onClose…) onto the OPN app manifest, forwards to shell.
2. Shell iframe host — lb apps are the first consumer of the third-party
   iframe seam (§10).
3. Injected-globals shim inside the iframe, implemented over the SDK:
   `sendNotification`→`notify`, contact selector→`directory`,
   gallery/`uploadMedia`→`media`, `getSettings`/`settings` mapped from shell
   settings (unsupported fields stubbed), popup/context menu→shell overlays.
   `fetchNui`/`useNuiEvent` work natively (iframe posts to its own resource
   origin) — the shim only supplies the helper globals.

Effort tiers, in order: core shim (manifest + globals + settings +
notifications + popup/context menu) → pickers (gallery, emoji, gif, color,
contact, share) → `createGameRender` (WebGL game feed, photo/video capture) —
heaviest, only camera-style apps need it.

Known ceiling: lb's API is unversioned and partly undocumented — the compat
target is "runs the official template + the popular free apps", never 100%.
Clean-room: reimplement signatures only. Bonus: the shim doubles as the spec
draft for our own third-party SDK — lb's surface is field-proven for what
apps actually need.

## 11. Coolify deployment

- Coolify runs on the FXServer host (or same DC): data-plane RTT rides the
  same wire as game traffic — the phone can never feel more remote than the
  server itself. RTT only returns as a concern for centrally-hosted
  multi-tenant Cores (§12.3).
- `opn-core` as a Docker Compose app: `core` (replicas: 2 when needed),
  one-click Postgres 16, Redis, MinIO. Traefik labels: TLS + sticky WS.
- Git-push deploys, health checks gating rollout, scheduled Postgres backups
  to S3-compatible, scheduled task for retention purge (partition drops).
- Skipped deliberately at this scale: Kafka, k8s, microservices, multi-region.
  Coolify multi-server covers the next 100×.
- **Host CPU: Intel i5-14500** (6 P-cores + 8 E-cores). FXServer is
  main-thread-bound — it gets the P-cores, uncontended. The OPN stack is
  pinned to E-cores via Docker `cpuset` (e.g. cores 12–19) so phone infra can
  never steal game-thread boost headroom; 8 E-cores dwarf the stack's needs.
  Cap tokio worker threads (`TOKIO_WORKER_THREADS=4`) — the default spawns
  one per logical core (20), wasteful in a pinned container.

## 12. Product seams (built now, exploited later)

1. **Transport interface** in the SDK: `DirectWss` is the only impl we build;
   a `GameTunnel` impl can be added for buyers who can't host infra.
2. **Framework bridge layer** in `opn-fivem`: flat global contract,
   one guarded impl per framework; we implement ours only.
3. **Tenancy** already isolates data per server — hosting a paid multi-tenant
   Core ("phone as a service") is a config change, not a redesign.

## 13. Decision records

**ADR-1 — Rust for the Core** (decided). At our scale (~300 concurrent,
~30 msg/s peak) player-perceivable perf is a wash vs Go/TS: median end-to-end
gain <1% (RTT dominates), p99 gain ~5–15% (no GC outliers). The decisive
numbers are footprint and ops: ~30–60 MB RAM static binary vs ~150–400 MB for
Node (5–10×), GC-free flat tail latency with zero tuning, compiler-checked
concurrency in the fan-out path, and the product story. Accepted cost:
~1.5–2× Core build time (paid once on a bounded ~8-primitive surface) and
`ts-rs` codegen instead of natively shared TS types.

*Amended 2026-07: project goal is explicitly stability + performance, not
shipping velocity — the FiveM ecosystem's gap is reliable, well-engineered
resources, not more of them faster. This removes ADR-1's only accepted cost
from the objective function: build-time tax is not a risk, it's the point.
Consequence: verification effort (property tests, protocol fuzzing, soak
tests — see OPN-CORE.md §15) is budgeted work, not gold-plating.*

**ADR-2 — Postgres, not Cassandra/NoSQL.** Cassandra's value (linear write
scaling, ring availability) exists only at multi-node, huge-write scale we
never reach (ceiling 100k registered; Discord didn't need it until billions of
messages). It blows the §7.1 budget (JVM, 1–4 GB heap, compaction/repair CPU)
and fights our primitives: `ledger` needs ACID, per-channel `seq` needs a
transactional counter, idempotency needs unique indexes, and composable apps
need ad-hoc queries — all free in Postgres, all hand-built and race-prone in
Cassandra. Escape hatch: the append-heavy `messages` store sits behind the
`channels` contract; it alone could move to Scylla later as a contained change.

**ADR-3 — No dedicated LB tier, no message broker.** Traefik (already required
for TLS) IS the load balancer: health checks, sticky WS, multi-server via
Coolify. A second LB layer balances nothing at 1–2 replicas. Kafka/RabbitMQ
protect nothing here: durability point is persist-then-ack in Postgres, there
is one consumer (the Core), and peak is ~30 msg/s. Redis covers the real async
needs — pub/sub for cross-replica fan-out, Streams for offline inbox/notify
queues. Rule on the wall: **every box must justify its idle cost.**

**ADR-4 — WS gateway is a module in the Core binary, not a service.** A
separate gateway pays an internal network hop + serialization on every message
to buy independent scaling we don't need. The seam stays in code (gateway
module ↔ primitive modules) so extraction later is a build change, not a
rewrite.

**ADR-5 — lb-phone compat as a shim package, not a design constraint.** We
adopt lb-phone's third-party app surface (AddCustomApp + injected iframe
globals) as a compatibility target because lb apps are self-contained
resources that never touch our data plane — compat costs a shell-services
shim (§10.2), not architectural concessions. OPN's own manifest/SDK stays the
primary contract; the shim is an optional adapter package built after the MVP
shell. Accepted cost: chasing an unversioned, partly undocumented API —
mitigated by targeting the official template + popular free apps as the test
corpus, and by the shim doubling as the field-proven spec draft for our own
iframe SDK.

## 14. Open questions (next brainstorm)

1. ~~Number portability & device model~~ — **decided: number lives on the
   character.** One character, one number, for life; devices are dumb
   hardware, phone items carry only a `device_id`. Simplest bridge contract,
   contacts never break, robbery/trade of a phone never moves identity.
   Accepted cost: no burner-via-device gameplay — if ever wanted, burners
   become *virtual numbers* (a `directory`/app-layer feature over the same
   `resolve(number)` choke point), not a schema migration. Freed numbers
   (character deletion) still enter a 30-day cooldown before recycling.
2. ~~`ledger` ↔ framework money~~ — **decided: two authorities, disjoint
   domains, explicit exchange** (Steam-wallet model). Framework owns
   cash/bank forever — the phone bank app is pass-through over the game
   plane, Core stores zero bank rows. Core `ledger` owns phone-native money
   (crypto, marketplace wallet, app credits) forever — the framework never
   sees it. The only crossing is explicit deposit/withdraw through the
   bridge (debit source authority first, credit second, idempotent both
   sides, journaled — see OPN-CORE.md §10.5). Escrow exists only in
   `ledger`; Marketplace auto-exchanges at checkout (deposit + hold composed
   in one flow). Invariant bought: no balance ever has two masters.
3. ~~Media pipeline~~ — decided, see §7.2: game-render capture in NUI,
   client-side thumbnails, no server-side image processing.
4. Push-to-Lua surface: exact contract for shell → client Lua (HUD toasts,
   flashlight, vibration/haptics, wallpaper on world props?).
5. ~~MVP app cut~~ — **decided.** v1 ships exactly five apps: **Settings,
   Contacts, Messages, Dialer (voice + video calls), Camera.**
   Messages scope: SMS + group chats, media (photo/video/GIF), voice
   messages, reactions, pinned messages, delivery + read receipts with
   timestamps, last seen, full notify integration. Social/feed apps and
   guild-style chat are deferred — the `feed` primitive stays designed and
   built into Core, but no first-party app exercises it in v1. Build order
   (each app hardens what the next depends on): Settings → Contacts →
   Messages (hot path soaks longest) → Camera → Dialer, video calls last
   (only step with new infra: WebRTC signaling + coturn).
