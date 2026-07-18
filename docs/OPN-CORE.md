# OPN-CORE — Core Design (v0.1)

Detailed design of `opn-core`, the Rust backend. Companion to [OPN.md](OPN.md);
this doc assumes its decisions (two planes, ~8 primitives, Rust modular
monolith, multi-tenant, Postgres/Redis/MinIO, contracts generated from Rust)
and drills into *how the binary is built*. Refinements to OPN.md decisions are
recorded as CDRs (§16).

---

## 1. Scope & non-goals

**In scope**: process/crate layout, WS gateway internals, command dispatch,
fan-out path, per-primitive schemas and commands, auth/authz, rate limiting,
config, observability, testing.

**Out of scope**: UI/SDK internals (opn-ui), Lua runtime (opn-fivem),
deployment specifics beyond what the binary needs (OPN.md §11).

**Non-goals, restated as constraints**:
- No service extraction, no broker, no dedicated LB (ADR-3/4). Every module
  boundary here is a *code* seam, not a network seam.
- Footprint budget from §7.1 governs: if a mechanism idles CPU or holds RAM
  proportional to anything but active connections, it needs a justification.

---

## 2. Process model & crate layout

One binary, one tokio runtime, N replicas (N=1 until measured otherwise).
Cargo workspace:

```
opn-core/
  crates/
    contracts/            # ALL wire types. serde + ts-rs derives.
      src/
        envelope.rs       # ClientMsg / ServerMsg
        cmd.rs            # Cmd enum (all commands, tagged union)
        evt.rs            # Evt enum (all pushed events)
        types/            # per-primitive DTOs: channels.rs, feed.rs, ...
        error.rs          # ErrCode
      bin/export_ts.rs    # writes .d.ts → published as @opn/contracts
    core/                 # the binary
      src/
        main.rs           # config → pools → router → serve
        config.rs         # env-only config (§13)
        state.rs          # AppState: PgPool, Redis, SessionRegistry, cfg
        gateway/
          ws.rs           # upgrade, auth, connection loop
          conn.rs         # per-connection task, send queue
          subs.rs         # topic → subscribers registry, fan-out
          link.rs         # tenant link (FXServer ↔ Core events, §5)
        http/             # axum routers: sessions, history reads, health
        primitives/
          identity/  channels/  feed/  calls/
          ledger/    media/     directory/  notify/
          # each: mod.rs (handle), store.rs (sqlx), + tests
        infra/
          auth.rs         # JWT mint/verify, API key check
          ratelimit.rs    # token buckets
          ids.rs          # UUIDv7 helpers
      migrations/         # sqlx migrate, plain SQL files
```

Rules:
- `contracts` has **no** dependency on `core`. It is the public surface;
  `export_ts` output is committed and semver-published (OPN.md §10.1).
- Primitives depend on `infra` and `contracts`, never on each other's
  `store.rs`. Cross-primitive calls go through the owning module's `pub fn`s
  (e.g. `notify::push(...)` is how channels reaches notify). Keeps the
  extraction seam of ADR-4 real without any indirection machinery — no
  traits-for-one-impl, no internal message bus.

---

## 3. Runtime anatomy

Steady-state tasks in the process:

| Task | Count | Purpose |
|---|---|---|
| axum server | 1 | HTTP + WS upgrade |
| connection task | 1 per WS conn | read loop + command dispatch |
| connection writer | 1 per WS conn | drains send queue → socket |
| tenant link task | 1 per connected tenant | §5 |
| Redis pub/sub listener | 1 (only if replicas > 1) | cross-replica fan-out |
| janitor | 1 | expired sessions, stale presence, hold timeouts; ticks every 30 s |

No other background work. No per-frame/interval polling anywhere; everything
else is request-driven. At 300 concurrent players that is ~600 lightweight
tasks — noise for tokio.

Shared state (`AppState`) is `Arc`-cloned into handlers: `PgPool` (sqlx,
max ~20 conns), Redis connection manager, `SessionRegistry` (§4.2),
per-process rate-limit table, config. No global mutable state outside the
registry; primitives are stateless functions over the pools.

---

## 4. WS gateway

### 4.1 Connection lifecycle

```
GET /ws  → 101 upgrade (unauthenticated)
  → first frame MUST be { cmd: "auth", payload: { token } } within 3 s
    (else close 4401; any other first frame → close 4400)
  → verify JWT (exp, sig, session not revoked)
  → register in SessionRegistry keyed by session_id → ack → normal protocol
  → spawn reader task + writer task
  → on close/error: unregister, drop subscriptions, presence TTL lapses
```

Token travels in the first WS frame, never in the URL — query strings land
in proxy and access logs (browser WS API can't set headers, so first-message
auth is the clean option for CEF). Pre-auth connections are capped (per-IP
and global) and hold no registry state.

- One WS per NUI session. A second connect with the same `session_id` kills
  the old one (last-writer-wins — handles CEF reloads cleanly).
- Heartbeat: server ping every 30 s, close after 2 missed pongs. Client-side
  reconnect with 0–3 s jitter is the shell's job (OPN.md §7).
- Auth is checked **at connect only** for the WS itself; `auth.refresh` (§11)
  exists to keep a fresh JWT for HTTP calls and reconnects.

### 4.2 SessionRegistry & subscriptions

In-process maps, sharded (`DashMap` or 16-way `Mutex<HashMap>` — measure,
don't guess; start with DashMap):

```
sessions:  session_id → ConnHandle { sender, world_id, character_id, device_id, subs }
topics:    (world_id, topic) → SmallVec<session_id>
```

- Topics are strings namespaced by primitive: `ch:<channel_id>`,
  `feed:<world>`, `call:<call_id>`, `notify:<device_id>`,
  `presence:<character_id>`. World id is part of the key — tenants can never
  cross-subscribe by construction.
- **Presence is per-character, never world-wide** (a world topic would leak
  every player's online state to everyone). Apps sub `presence:<char>` for
  characters currently on screen (thread header, contacts list) — mount/
  unmount pattern. Payload `{ online, last_seen_at? }`, snapshot-on-sub
  (CDR-6). Gated by `characters.share_presence` at sub time: toggle off →
  snapshot `{ online: null }`, no events — one switch hides both the online
  dot and last seen.
- `sub` is authorized by the owning primitive (e.g. `channels` checks
  membership before the topic registration happens). `unsub` and disconnect
  remove entries. Shell subscribes `notify:*` for its device on connect; apps
  sub/unsub on mount/unmount (OPN.md §4).

### 4.3 Send queue & backpressure

Per connection: bounded `mpsc` (capacity 256) between fan-out and the writer
task. Every outbound event is tagged at the contracts level:

- **durable** (messages, receipts, ledger events): if the queue is full,
  close the connection. The client reconnects and resumes by seq — nothing
  is lost, and a client that slow is effectively dead anyway.
- **ephemeral** (typing, presence): dropped silently when the queue is
  ≥ 80% full. Never queued at the cost of durable events.

This is the whole backpressure story. No per-topic queues, no priority
wheels — at 30 msg/s peak the queue exists for pathological clients, not
throughput.

### 4.4 Resume

`sub { topic, last_seq? }`:
- `ch:*` topics: replay `WHERE channel_id = $1 AND seq > $2 ORDER BY seq`
  from Postgres (capped at 500 rows; beyond that the client is told to do an
  HTTP history fetch instead — reconnect after a week isn't "resume").
- `feed:*`, `notify:*`: no seq replay; client re-fetches via HTTP, events are
  advisory ("something changed") not the source of truth.
- `presence`, `call:*`: snapshot-on-sub (current state pushed as first
  event), no history.

Rule: **replayable topics are exactly the ones backed by a seq column.**
Everything else is snapshot + advisory, and the UI must be able to cold-load
it over HTTP.

---

## 5. Tenant link

OPN.md §6 has Core emitting events the FXServer gateway resource consumes
(voice targets on call accept). That needs a server-to-server push channel:

- Gateway resource opens `wss://core/link` authenticated by tenant API key
  (header, not query param). One connection per FXServer, auto-reconnect.
- **Version handshake**: link hello carries
  `{ resource_version, contracts_version }`; Core logs the pair and refuses
  only known-broken combos (close 4409 + reason). No compatibility policy
  beyond that until multi-tenant hosting exists — the handshake field is the
  seam, enforcement slots in later without protocol change (closes §17 Q4).
- **Down** (Core → FXServer): tenant-scoped events only —
  `calls.voice { call_id, action: set_targets|clear, characters[] }`,
  `admin.*` later. Same envelope as the client protocol, same contracts crate.
- **Up** (FXServer → Core): nothing. All FXServer→Core calls are plain HTTPS
  with the API key (token mint, admin ops). Keeping the link one-directional
  means no request/response state machine on it.
- Link down = calls still connect (state machine lives in Core) but voice
  targets lag until reconnect; events for a disconnected tenant are dropped,
  and the gateway resource re-syncs active calls on link connect
  (`GET /v1/tenants/self/calls/active`).

---

## 6. HTTP API surface

Small on purpose. WS = commands + events; HTTP = auth bootstrap, bulk reads,
and anything a browser cache should handle.

| Route | Auth | Purpose |
|---|---|---|
| `POST /v1/tenants/self/sessions` | API key | mint session JWT (OPN.md §3) |
| `GET  /v1/tenants/self/calls/active` | API key | tenant link re-sync (§5) |
| `POST /v1/tenants/self/exchange` | API key | bank↔wallet exchange leg (§10.5) |
| `GET  /v1/tenants/self/exchange?since` | API key | exchange journal for reconciliation |
| `GET  /v1/channels/:id/messages?before_seq&limit` | JWT | history pagination |
| `GET  /v1/feed?cursor` / `GET /v1/feed/posts/:id/comments` | JWT | feed reads |
| `GET  /v1/notify/inbox?cursor` | JWT | offline inbox on login |
| `GET  /healthz` | none | liveness + deep check (PG/Redis ping) |
| `GET  /metrics` | internal | Prometheus (bound to internal interface) |

Bulk reads are HTTP (not WS commands) so they get browser caching, ranges,
and don't head-of-line-block the WS. Media bytes never touch these routes —
presigned MinIO URLs only (OPN.md §7.2).

**Pagination idiom (all paginated reads, CDR-7):** opaque cursor. Response:
`{ items, next_cursor: string | null }`; request: `?cursor=...&limit=...`
(limit capped server-side). The cursor is base64 of the keyset tuple
(`(created_at, id)` today) — internally it's keyset pagination
(`WHERE (created_at, id) < ($1, $2) ORDER BY created_at DESC, id DESC`),
externally it's a string clients echo back. Decode failure → `invalid`;
clients restart from the top. Same contract type for feed, history, inbox,
gallery, ledger history.

---

## 7. Command dispatch & error model

No dynamic registry. Commands are **one tagged Rust enum** in `contracts`;
serde does the routing, `match` does the dispatch, the compiler does the
exhaustiveness:

```rust
#[derive(Deserialize, TS)]
#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]
pub enum Cmd {
    Sub { topic: String, last_seq: Option<i64> },
    Unsub { topic: String },
    AuthRefresh,
    ChannelsSend { channel_id: Uuid, client_uuid: Uuid, body: MessageBody },
    ChannelsMarkRead { channel_id: Uuid, up_to_seq: i64 },
    // ... one variant per command, ~40 total at MVP
}
```

Dispatch in the connection task:

```
parse envelope → rate-limit check (§12) → match variant
  → primitives::<owner>::handle_x(ctx, payload) → ack { reply_to, ok, payload }
```

`ctx` carries the authenticated `(tenant_id, world_id, character_id,
device_id)` from the registry — **handlers never read identity from the
payload** (OPN.md §9).

Errors: one `ErrCode` enum in contracts —
`unauthorized | forbidden | not_found | invalid | conflict | rate_limited |
too_large | internal`. Ack shape:
`{ reply_to, ok: false, err: { code, msg } }`. `msg` is developer-facing;
UI copy is the app's job keyed off `code`. Internal errors log the detail and
return only `internal` — no stack traces to an untrusted NUI.

Commands are handled **sequentially per connection** (the read loop awaits
each handler). This is deliberate: it gives per-user ordering for free, and
at ≤ 5 ms p99 per command no user can feel it. Cross-user concurrency comes
from having one task per connection.

---

## 8. Fan-out path

The hot path, end to end (channels.send as the example):

```
1. handler: BEGIN
     seq = UPDATE channels SET last_seq = last_seq + 1
           WHERE id = $1 RETURNING last_seq          -- row lock = per-channel serialization
     INSERT INTO messages (id, channel_id, seq, sender, body, client_uuid, ...)
       ON CONFLICT (channel_id, client_uuid) DO NOTHING
     -- conflict → SELECT existing row, skip step 3 (idempotent retry, same ack)
   COMMIT
2. ack SENT to sender { message_id, seq }             -- persist-then-ack (OPN.md §5)
3. fan-out:
   a. subs::publish(world, "ch:<id>", evt)            -- local registry, direct to send queues
   b. if replicas > 1: PUBLISH opn:<world>:ch:<id>    -- Redis, other replicas do (a)
   c. notify::route(offline_members, evt)             -- inbox rows for members with no live session
```

Notes:
- The per-channel `seq` via `UPDATE … RETURNING` inside the insert
  transaction is the ordering mechanism. Contention on the channel row is the
  serialization point — correct, and irrelevant at our write rates
  (a "hot" channel at 10 msg/s holds the lock microseconds at a time).
- Step 3 is after COMMIT, fire-and-forget from the sender's perspective. A
  crash between 2 and 3 loses only the live push; the row is durable and
  arrives via resume/inbox. That is the documented delivery guarantee:
  **at-least-once to the UI, exactly-once in storage** (client dedupes by
  `message_id`, renders by seq).
- Redis pub/sub messages carry the full event payload (already serialized
  once) — the receiving replica deserializes into the same `Evt` and hits its
  local registry. No DB read on the subscriber side.

---

## 9. Data layer conventions

- **IDs**: UUIDv7 everywhere (time-ordered → index-friendly, no central
  sequence, safe to expose). Generated in `infra::ids`.
- **Every domain row carries `world_id`** (OPN.md §3), and every query filters
  by it — composite indexes lead with `world_id`. Enforced twice: by
  convention in queries, and by **Postgres RLS as backstop** (CDR-3) — each
  request transaction runs `SET LOCAL app.world_id`, policies on domain
  tables filter to it. A forgotten `world_id` predicate then returns
  correct-but-slower rows instead of leaking another server's data.
- **Timestamps**: `timestamptz`, set by Postgres (`now()`), never by clients.
- **Migrations**: `sqlx migrate`, plain SQL, forward-only, run on startup
  (single replica) / by deploy job (multi-replica later).
- **Queries**: `sqlx::query!` compile-checked against the schema;
  `cargo sqlx prepare` output committed so CI builds offline.
- **Partitioning**: only `messages`, range-partitioned by month on
  `created_at`, per OPN.md §7. Janitor-adjacent scheduled task (Coolify cron
  hitting an internal admin route, or plain `pg_cron`) creates next-month and
  drops expired partitions. Nothing else is remotely big enough to partition.
- **No ORM, no repository traits.** `store.rs` per primitive is a flat set of
  `pub async fn`s taking `&PgPool` (or `&mut Transaction` where composition
  needs it — ledger + marketplace escrow in one tx).

---

## 10. Primitives

Per primitive: tables → commands (WS) → events (pushed) → notes. Types live
in `contracts`; names below are the wire names.

### 10.1 identity

Tables:
```
tenants        (id, name, api_key_hash, allowed_origins[], world_id, created_at)
worlds         (id, name, created_at)
characters     (id, world_id, framework_ref, number, last_seen_at,
                share_presence bool default true, settings jsonb, created_at)
               -- framework_ref = "<framework char id>", unique per world
               -- number: text, UNIQUE (world_id, number) WHERE number IS NOT NULL;
               --         assigned on first device registration, held for life
devices        (id, world_id, owner_character, kind, settings jsonb, created_at)
               -- pure hardware: notify routing endpoint + settings scope, no number
app_accounts   (id, world_id, character_id, app_id, handle, meta jsonb, created_at)
               -- UNIQUE (world_id, app_id, handle)
sessions       (id, tenant_id, world_id, character_id, device_id, created_at, expires_at, revoked_at)
```

Commands: `auth.refresh`, `identity.me` (resolve own device/accounts),
`identity.app_login { app_id, account_id }` (switch active app account —
stored per session, not per app code, OPN.md §3),
`identity.get_settings / set_settings { scope: device | character }`.

**Settings storage**: `settings jsonb` column on `devices` (wallpaper,
ringtone, airplane, per-app toggles) and `characters` (cross-device prefs).
Opaque to Core — schema belongs to the shell/SDK, Core validates only a size
cap (16 KB) — so reskins add toggles without contract bumps (§10.1 goal).
One exception, promoted to a real column because Core itself enforces it:
`characters.share_presence bool` (the last-seen/online privacy toggle,
gates presence subs below).

HTTP: session mint (§6) upserts `characters` by `(world_id, framework_ref)`
on first sight — no separate registration flow.

Notes: number-on-character is decided (OPN.md §14.1). `directory.resolve`
stays the single number → character choke point — nothing else reads
`characters.number` directly, so future virtual numbers (burner-as-feature)
slot in behind it without touching callers. Freed numbers (character
deletion) sit in a 30-day cooldown before reuse; the janitor enforces it via
a `retired_numbers (world_id, number, freed_at)` table consulted at
assignment.

### 10.2 channels

Tables:
```
channels        (id, world_id, kind, name, meta jsonb, last_seq bigint, created_at)
                -- kind: sms | group | dm | match | mail
channel_members (channel_id, character_id, joined_at,
                 last_delivered_seq bigint, last_read_seq bigint, muted bool)
messages        (id, channel_id, seq, sender_character, body jsonb, client_uuid, created_at)
                -- PARTITION BY RANGE (created_at); UNIQUE (channel_id, client_uuid); UNIQUE (channel_id, seq)
reactions       (message_id, character_id, emoji, created_at)
channel_pins    (channel_id, message_id, pinned_by, pinned_at)
                -- PK (channel_id, message_id); ≤ 50 pins/channel, enforced at pin time
```

Commands: `channels.open_direct` (pair threads), `channels.create` (groups:
creator picks members, kind=group),
`channels.send`, `channels.mark_delivered`, `channels.mark_read`,
`channels.typing`, `channels.react`, `channels.pin/unpin`,
`channels.member_add/remove`, `channels.list` (own memberships snapshot).

Events on `ch:<id>`: `channels.message`, `channels.receipt`
(`{ character, up_to_seq, kind: delivered|read, at }`), `channels.typing`
(ephemeral), `channels.reaction`, `channels.pin`, `channels.member`.

Notes:
- `body` is jsonb `{ text?, media_ids?, gif_url?, meta? }` — one message
  shape for all kinds; apps interpret. Voice messages are just
  `media_ids: [<kind=audio>]`. `gif_url` is host-allowlisted at send time
  (Tenor-style external providers; no storage cost) — uploaded GIFs go
  through `media` like any file. Attachment authz: media ids are validated
  as owned by sender at send time.
- Receipts are watermark-based, never per-message rows:
  `last_delivered_seq` (set by the client on receiving events — automatic,
  fires even with the app closed since the shell holds the WS) and
  `last_read_seq` (set on viewing the channel). Receipt events carry `at`
  timestamps for the UI; the row stores only the seqs. Group-chat display
  ("delivered to all / read by all") = min over members, computed client-side
  from the per-member receipt events.
- **Last seen**: `characters.last_seen_at`, written once on WS disconnect
  (live "online" state comes from Redis presence; the column only answers
  "when were they last online"). Per-character privacy toggle in Settings
  gates it at read time.
- SMS/DM threads are found-or-created **pairs only**:
  `channels.open_direct { number }` with a unique index on the ordered
  character pair — no duplicate threads, no member-set hashing. Groups are
  always explicit `channels.create`.

### 10.3 feed (post-MVP — primitive built, no first-party app in v1)

Tables:
```
posts     (id, world_id, app_id, author_account, body jsonb, media_ids uuid[],
           like_count int, comment_count int, created_at)
follows   (world_id, app_id, follower_account, followee_account, created_at)
likes     (post_id, account_id, created_at)                    -- PK (post_id, account_id)
comments  (id, post_id, author_account, body, created_at)
hashtags  (tag, post_id, world_id, app_id)
```

Commands: `feed.post`, `feed.delete`, `feed.like/unlike`, `feed.comment`,
`feed.follow/unfollow`. Reads are HTTP (§6): home timeline is
**fan-out-on-read** — `posts JOIN follows` with `(world_id, app_id,
author_account, created_at)` index. Precomputed timelines are a
100k-registered problem, not a 2k one (CDR-2).

Events on `feed:<world>:<app>`: advisory only — `feed.activity
{ kind: post|like|comment, post_id, actor }`. Clients refresh what they're
looking at; notification of *your* post being liked routes through `notify`.

Counters (`like_count`) are denormalized atomic `UPDATE … SET n = n + 1` in
the same tx as the likes insert — cheap, exact, no drift.

### 10.4 calls

Tables:
```
call_sessions     (id, world_id, kind, state, created_at, ended_at)
                  -- kind: voice | video;  state: ringing | active | ended
call_participants (call_id, character_id, device_id, state, joined_at, left_at)
                  -- state: ringing | joined | declined | left
```

Commands: `calls.start { callee_number, video: bool }`, `calls.accept`,
`calls.decline`, `calls.hangup`,
`calls.signal { call_id, to, payload }` — opaque WebRTC signaling relay
(offer/answer/ICE) for video calls; Core validates sender/recipient are
active participants and forwards, never inspects the payload. Video bytes go
P2P (STUN) or via the coturn relay (OPN.md §6) — never through Core.

Events: on `call:<id>` for participants (`calls.state` full-session
snapshots — small, simpler than deltas; plus `calls.signal` relays); ring
delivery via `notify` (device topic) so the dialer app doesn't need a
standing subscription; voice-target events via tenant link (§5). Voice audio
for both kinds stays in pma-voice — WebRTC carries video only.

State machine is enforced in the handler (illegal transition → `conflict`).
Sessions with `state != ended` and no joined participants are reaped by the
janitor after 60 s — no zombie rings after crashes.

### 10.5 ledger

Tables:
```
accounts  (id, world_id, owner_kind, owner_id, currency, balance bigint, created_at)
          -- owner_kind: character | app_account | system;  CHECK (balance >= 0)
transfers (id, world_id, from_account, to_account, amount bigint, kind, ref jsonb,
           client_uuid, created_at)                -- UNIQUE (from_account, client_uuid)
holds     (id, account_id, amount bigint, state, ref jsonb, created_at, expires_at)
          -- state: held | captured | released
```

Commands: `ledger.transfer { to, amount, client_uuid, ref }`,
`ledger.hold { amount, ref }`, `ledger.capture { hold_id, to }`,
`ledger.release { hold_id }`, `ledger.history` (HTTP for pagination).

Semantics:
- Transfer = one tx: debit `UPDATE … balance = balance - $n` (CHECK enforces
  funds), credit, insert transfer row. Idempotent by `client_uuid`.
- Available balance = `balance - SUM(active holds)` — checked at
  hold/transfer time in the same tx. Escrow (marketplace) = hold → capture.
- Expired holds auto-release via janitor.
- `balance` is authoritative; `transfers` is the audit trail. The invariant
  (`per-account: SUM(credits) − SUM(debits) == balance`) is enforced as a
  **nightly reconciliation job** (janitor task): recompute from `transfers`,
  compare to `balance`, alert + freeze the mismatched account on drift
  (freeze = `conflict` on outgoing ops until a human looks). Silent
  corruption becomes detected corruption within 24 h. Full double-entry
  postings remain the upgrade path if regulation-grade audit ever matters —
  it will not, this is a game.
- **Framework money split (decided, OPN.md §14.2)**: framework is sole
  authority for cash/bank (phone bank app = game-plane pass-through, no Core
  rows); `ledger` is sole authority for phone-native money (crypto, wallet,
  app credits). The only crossing is the **exchange protocol**:
  - Deposit (bank → wallet): bridge debits framework bank in a framework tx,
    journals `{exchange_id, char, amount, direction}` in its own storage,
    then calls `POST /v1/tenants/self/exchange` (API key,
    idempotency = `exchange_id`); Core credits the character's wallet from
    the tenant `system` account. Withdraw: same, reversed — Core debits
    wallet first (`ledger.withdraw`, held until bridge confirms), bridge
    credits bank, confirms; unconfirmed withdrawals auto-release via hold
    expiry.
  - Rule: **debit the source authority first, credit second, both sides
    idempotent on `exchange_id`.** Crash between the two legs = journaled
    intent the bridge replays on restart; replay hits idempotency and
    resolves to exactly-once. No leg is ever inferred — only replayed.
  - Marketplace checkout auto-composes deposit + hold in one user action;
    both steps are the same idempotent primitives, no new mechanism.
  - The nightly reconciliation job (above) also checks `system` account
    movements against the bridge journal via
    `GET /v1/tenants/self/exchange?since` — cross-authority drift is
    detected, not assumed away.

### 10.6 media

Tables:
```
media (id, world_id, owner_character, kind, mime, bytes int, object_key, thumb_key?,
       state, favourite bool, created_at)      -- state: pending | live;  keys are content-addressed
```

Commands: `media.request_upload { kind, bytes, mime }` → presigned PUTs
(original + thumb) with size/MIME conditions per OPN.md §7.2 — kinds:
photo ≤ 2 MB, video ≤ 25 MB, **audio ≤ 1 MB (voice messages, no thumb)**;
`media.commit { media_id }` → verifies object exists (HEAD), state → live;
`media.favourite`, `media.list` (own gallery, HTTP).

Notes: pending rows older than 15 min reaped by janitor (orphan uploads).
`media.commit` does not verify synchronously; instead the janitor's sweep
**verifies live rows against MinIO** (object exists, size ≤ declared bytes,
via batched HEADs) — a row whose object is missing or oversized is flagged
and reverted to `pending`, so presign-cap bypasses can't persist. Retention
is MinIO lifecycle + the same pass marking rows whose objects expired.
Nothing in Core ever reads object bytes.

### 10.7 directory

Tables:
```
contacts  (owner_character, world_id, number, display_name, avatar_media?, meta jsonb)
          -- PK (owner_character, number): contacts point at numbers, resolved
          -- to a character only at action time via directory.resolve
blocks    (world_id, blocker_character, blocked_number)
listings  (id, world_id, app_id, kind, title, body jsonb, contact_number, created_at, expires_at)
```

Commands: `directory.contacts.*` (CRUD), `directory.block/unblock`,
`directory.resolve { number }` → opaque routing id (never leaks the character
behind a number), `directory.listings.*` (YellowPages/ads).

Blocks are enforced where actions happen: `channels.open_direct` and
`calls.start` consult `blocks` — directory owns the data, not the
enforcement points.

### 10.8 notify

Tables:
```
inbox (id, world_id, character_id, device_id?, app_id, kind, payload jsonb,
       seen_at?, created_at)
```

API (internal to Core — other primitives call `notify::route`):
route(recipient, notification) →
- live session exists → push `notify.event` on `notify:<device_id>` (shell
  renders toast / badge; airplane-mode gating is client-side, OPN.md §5);
- no live session → insert `inbox` row, read via HTTP on next login.

Commands: `notify.seen { ids }`, `notify.clear`.

**Routing policy:**
- **Notification class** — every notification carries
  `class: ring | alert | silent`, chosen by the emitting primitive (calls →
  ring, messages → alert, receipts/likes → silent). Class is **semantic
  urgency only — Core mandates zero presentation**: what a `ring` looks like
  (fullscreen, pill, toast) is entirely the shell's choice; the reference
  shell's mapping is a default, reskins override freely. Same for sounds,
  DND behavior, badge styling.
- **Suppression split** — Core honors what it already knows: a muted channel
  (`channel_members.muted`) routes with `class: silent` (data flows, alert
  urgency stripped — thread still accumulates unread). Everything
  player-local (per-app toggles, DND, airplane gate) is enforced by the
  shell at render time; Core never stores or evaluates it.
- **Badges are derived, never stored** — unread per channel =
  `last_seq − last_read_seq`, per app = sum over its channels; inbox badge =
  unseen inbox rows. No counter columns anywhere; counters drift,
  derivations can't.

CDR-1: inbox lives in **Postgres, not Redis Streams** (refines ADR-3's
sketch). Reasons: inbox must survive Redis restarts (Redis here is cache-tier,
unpersisted), volume is trivial (≤ tens of rows/user/day), and login-time
reads want SQL (filter by app, unseen, cursor). Redis keeps exactly two jobs:
presence TTL keys and cross-replica pub/sub.

---

## 11. Auth & authz

- **JWT**: HS256, secret held only by Core (Core mints and verifies its own
  tokens — no third party ever validates them, so no need for asymmetric
  keys). Claims: `sid, tenant, world, char, device, exp` (10 min).
  `auth.refresh` over the live WS returns a fresh token (used for HTTP calls
  and reconnects); refresh also bumps `sessions.expires_at`. Revocation =
  `sessions.revoked_at` checked at connect + on refresh — a revoked session's
  live WS is killed via the registry.
- **Tenant API keys**: random 256-bit, stored hashed (SHA-256 — high-entropy
  keys need no KDF), shown once at creation. Sent in `Authorization` header.
- **Authorization is per-command in the owning primitive**, always against
  ctx identity + DB state (membership, ownership), never client-supplied ids
  (OPN.md §9). There is no central policy engine — a `channels.send` authz
  check is one indexed `channel_members` lookup inside the tx it already runs.
- **Origin checks**: WS upgrade and CORS validate `Origin` against the
  tenant's `allowed_origins` (plus the `cfx-nui-*` origin FiveM uses).

---

## 12. Rate limiting

Token buckets, in-process, keyed `(character_id, class)`:

| Class | Example commands | Budget |
|---|---|---|
| `msg` | channels.send, feed.comment | 1/s sustained, burst 5 |
| `social` | likes, follows, reactions, typing | 5/s, burst 20 |
| `money` | ledger.* | 1/s, burst 2 |
| `expensive` | media.request_upload, channels.create | 0.2/s, burst 3 |
| `read` | sub, list, HTTP reads | 10/s, burst 30 |

- Exceeded → `rate_limited` ack with `retry_after_ms`; never a disconnect
  (a buggy app must not kill the phone).
- In-process only: with sticky WS, a character's commands land on one
  replica, so distributed limit state buys nothing (CDR-4). Per-tenant
  aggregate ceilings (protect Core from one broken tenant) are enforced the
  same way on API-key routes.
- Buckets are lazily created, swept by the janitor — memory is proportional
  to *active* characters.

---

## 13. Config

Env-only (12-factor), read once at startup, fail-fast on missing:

```
OPN_BIND=0.0.0.0:8080          OPN_METRICS_BIND=127.0.0.1:9090
DATABASE_URL=postgres://...    REDIS_URL=redis://...
S3_ENDPOINT/S3_BUCKET/S3_KEY/S3_SECRET
OPN_JWT_SECRET                 OPN_SESSION_TTL_SECS=600
OPN_REPLICAS=1                 # >1 enables Redis pub/sub path
RUST_LOG=opn=info
```

Per-tenant config (origins, media caps overrides) lives in the `tenants`
row, cached in-process for 60 s. No config files, no hot reload — a restart
is sub-second and drops nothing (clients resume, §4.4).

---

## 14. Observability

- **Logs**: `tracing` + JSON to stdout (Coolify collects). One span per
  command with `cmd, tenant, world, char, duration, outcome`. Payload bodies
  are never logged.
- **Metrics** (Prometheus, `/metrics` on the internal bind): connections
  gauge, commands/s by cmd + outcome, command latency histogram (the §7 p99
  target is watched here), fan-out queue depth + drops (by class), Postgres
  pool in-use, Redis pub/sub lag, inbox insert rate.
- **Health**: `/healthz` does live PG `SELECT 1` + Redis `PING`; Coolify
  gates rollout on it (OPN.md §11).
- Alert-worthy at MVP: p99 command latency > 25 ms sustained, durable-queue
  disconnects > 0, PG pool exhaustion, janitor failures. That's the list;
  no dashboards-for-dashboards.

---

## 15. Testing

- **Unit**: state machines (calls, holds) and pure logic — plain `#[test]`.
- **Store/integration**: `#[sqlx::test]` against a real Postgres (per-test
  DB, migrations applied) — covers seq assignment, idempotency conflicts,
  balance checks. Redis/MinIO via docker compose in CI for gateway and media
  flow tests.
- **Protocol**: one WS harness test per primitive happy-path
  (connect → sub → cmd → ack → event), plus resume-after-drop and
  duplicate-`client_uuid` retry. These double as the living contract examples.
- **Contracts drift**: CI regenerates `.d.ts` and fails on uncommitted diff —
  the published types can never lag the Rust.

No mocked-Postgres unit tests: sqlx compile-time checking plus real-DB tests
make them redundant.

**Stability-grade verification** (budgeted work, not gold-plating — project
goal is stability + performance over velocity, ADR-1 amendment):
- **Property tests** (`proptest`) on the invariant-bearing logic: ledger
  (∑ transfers == balances, holds never over-release, no negative balance
  under concurrent interleavings), channels (seq strictly monotonic and
  gapless per channel, idempotent retry returns identical ack), calls state
  machine (no transition out of `ended`, no zombie participants).
- **Protocol fuzzing**: `cargo-fuzz` on envelope/command deserialization —
  the WS input is attacker-controlled by definition (untrusted NUI, §11);
  malformed input must cost an error ack, never a panic or task death.
- **Soak test**: simulated 10× design load (3k connections, 300 msg/s) for
  24 h before each release — watching for RAM creep (registry/bucket leaks),
  fd leaks, p99 drift. A phone that degrades on day 3 of server uptime fails
  the whole project premise.
- **Chaos drills, scripted**: kill -9 Core mid-send (persist-then-ack means
  zero loss — verify it), restart Postgres under load (pool recovery),
  restart Redis (presence rebuilds, pub/sub resubscribes), drop tenant link
  (call voice re-sync on reconnect). Each is a repeatable script in-repo,
  run in CI weekly, not a one-time manual exercise.

---

## 16. Core decision records

**CDR-1 — notify inbox in Postgres, not Redis Streams.** Refines ADR-3.
Durability across Redis restarts, trivial volume, SQL reads at login. Redis
scope narrows to presence + pub/sub only.

**CDR-2 — feed is fan-out-on-read.** No precomputed timelines, no fan-out
workers. A `posts JOIN follows` with the right index serves hundreds of DAU
indefinitely; materialized timelines are a contained later change behind the
same `feed` contract if the 100k ceiling is ever approached.

**CDR-3 — world isolation enforced by convention AND Postgres RLS.**
(Flipped 2026-07 under the stability-over-velocity goal; originally
convention-only.) Every query filters `world_id`, every topic key embeds it,
ctx is server-derived — and RLS policies on all domain tables
(`world_id = current_setting('app.world_id')`, set via `SET LOCAL` per
transaction) backstop the convention. Rationale: one missed filter in one
query is a cross-server data leak — the worst failure class for a
multi-tenant product — and RLS makes it structurally impossible at planner
cost that is unmeasurable at 30 msg/s. Accepted friction: migrations define
policies per table, tests run as the non-bypassing role.

**CDR-4 — rate limits are per-replica in-process.** Sticky WS pins a
character to one replica, so shared limit state adds a Redis round-trip per
command for zero enforcement gain. Revisit if stickiness is ever dropped.

**CDR-5 — commands execute sequentially per connection.** Per-user ordering
for free, no interleaving bugs, and handler p99 < 5 ms makes queuing
invisible. Parallelism across users comes from per-connection tasks.

**CDR-6 — snapshot events for low-frequency state (calls), delta events for
high-frequency streams (messages).** Snapshots kill delta-desync bugs where
rates are low; deltas + seq where volume demands it.

**CDR-7 — one pagination idiom: opaque keyset cursors.** (Decided 2026-07.)
Every paginated read returns `{ items, next_cursor }` where the cursor is a
base64-wrapped keyset tuple — keyset correctness (no skips/dupes under
concurrent inserts, index-walk cost) without leaking sort keys into the
public contract. Internals stay swappable (a ranked feed changes the cursor
payload, not the contract; stale cursors decode-fail to `invalid` and the
client restarts from the top — acceptable for every surface we paginate).
Rejected: offset (skips/dupes, O(n) scans), bare UUIDv7 cursors (binds the
contract to id ordering, hands untrusted clients fabricable handles). No
signing — decode failure is harmless and row access is gated by authz + RLS,
not by cursor possession.

---

## 17. Open questions (Core-level)

1. ~~`channels.open_direct` canonicalization~~ — decided with the MVP cut:
   **pairs only.** Groups are always explicit `channels.create` (creator +
   member list) — no member-set hashing, no ambiguity. Unique index on the
   ordered pair for sms/dm kinds.
2. ~~Guild-kind channels~~ — deferred entirely with guild-style apps
   (OPN.md §14.5); no `guild` kind in v1 schema. Design it when such an app
   is actually planned.
3. ~~Media HEAD-on-commit vs janitor sweep~~ — decided: async janitor
   verification (§10.6); commit stays fast, bypasses can't persist.
4. ~~Tenant link protocol versioning~~ — decided: version handshake in the
   link hello now (§5), enforcement policy deferred until multi-tenant
   hosting exists.
5. ~~Feed pagination cursor shape~~ — decided: opaque keyset cursors,
   uniform across all paginated reads (CDR-7, §6).
