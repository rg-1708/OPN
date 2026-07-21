# What you can build with opn-core

opn-core is one Rust binary that gives you a real-time communications backend —
messaging, calls, money, social feeds, media. Any client works: a pure web app, a
mobile app, a game (FiveM is one such consumer, not a requirement). You talk to it
over **one WebSocket**: send **commands** (each gets an ack), receive **events** on
topics you subscribe to. A few things (login, history reads, media) are plain HTTP.
Every tenant is an isolated world; identity is server-derived, so a client never
trusts a client-supplied id.

Below: each thing the core does, and how you use it.

---

## Auth — log a character in

Mint a session over HTTP with your tenant API key, then hand the token to the WS.

1. `POST /v1/tenants/self/sessions` with `{ framework_ref, device_id? }` and header
   `Authorization: Bearer <api_key>` → returns `{ token, session_id, character, device }`.
   `framework_ref` is just your external user id (a game char id, a web account id — whatever your app keys users by); core upserts a character for it on first sight.
2. Open `GET /ws`. **First frame must be** `auth { token }` within 3 s.
3. Token lives 10 min. Call `auth.refresh` over the live socket to get a fresh one — no reconnect.

Second login for the same identity kills the old socket (last-writer-wins). Every
later command runs as that character; you never pass ids for "who am I".

## Messaging — DMs and group chats

- **1:1 thread:** `channels.open_direct { number }` — found-or-created, no duplicates.
- **Group:** `channels.create { name?, members[] }` (≤32 members).
- **Send:** `channels.send { channel_id, client_uuid, body }`. `body` is `{ text?, media_ids?, gif_url? }` — same shape for texts, images, voice notes (audio media), GIFs. Retry with the same `client_uuid` = same message (idempotent).
- **Get events:** `sub ch:<channel_id>` → you receive `channels.message`, `channels.receipt`, `channels.typing`, `channels.reaction`, `channels.pin`, `channels.member`.
- **Read/delivered:** `channels.mark_read` / `channels.mark_delivered` (watermarks, not per-message). Typing: `channels.typing`. React/pin: `channels.react`, `channels.pin`.
- **History:** `GET /v1/channels/:id/messages?before_seq&limit`. On reconnect, `sub` with `last_seq` to replay what you missed (up to 500; past that, cold-load over HTTP).

Delivery is at-least-once to the UI, exactly-once in storage — dedupe by message id, order by `seq`.

## Calls — voice and video, 1:1 and group

- **1:1:** `calls.start { callee_number, video }`, then `calls.accept` / `calls.decline` / `calls.hangup`. The callee gets a ring through notifications (no standing subscription needed).
- **Video signaling:** `calls.signal { call_id, to, payload }` relays WebRTC offer/answer/ICE (≤16 KB, opaque — core forwards, never inspects). Media goes P2P/relay, never through core.
- **Group:** `calls.group.create { label?, max_participants? }` opens a LiveKit room and auto-joins you; others `calls.group.join` / `calls.group.leave`. Any character in the world holding the `call_id` can join (cap default 32, per-tenant room ceiling 50).
- **State:** `sub call:<call_id>` → full-snapshot `calls.state` / `calls.group.state` on every change. Illegal transition (accept a dead call, etc.) → `conflict`. Unanswered rings and dead calls get reaped automatically.

## Money — wallets, transfers, escrow

- **Transfer:** `ledger.transfer { to, amount, client_uuid, ref }`. Idempotent by `client_uuid`, can't overdraw (balance ≥ 0 enforced in-DB).
- **Escrow (marketplace):** `ledger.hold { amount, ref }` reserves funds, then `ledger.capture { hold_id, to }` pays out or `ledger.release { hold_id }` refunds. Available balance = balance − active holds. Unclaimed holds auto-release.
- **History:** `GET /v1/ledger/history?cursor`.

Balances are reconciled nightly against the transfer log; a mismatched account is frozen (outgoing ops → `conflict`) until a human looks.

## Exchange — bridge an external ledger into the wallet

Optional. Only if you have money living outside core (a game economy, a bank, any
external ledger) and want to move it in and out of the phone wallet. A trusted
server-side bridge (never the client) drives it over HTTP with the API key:
`POST /v1/tenants/self/exchange` (deposit = external→wallet, withdraw = wallet→external,
held until the bridge confirms). Idempotent on `exchange_id`. Read the journal for
reconciliation with `GET /v1/tenants/self/exchange?since`. Marketplace checkout
composes deposit + hold in one action. Skip this entirely if the wallet is your
only ledger.

## Media — photos, video, voice notes

Bytes never pass through core — you get presigned URLs.

1. `media.request_upload { kind, bytes, mime }` → presigned PUT(s) (photo ≤2 MB, video ≤25 MB, audio ≤1 MB). PUT the file straight to storage.
2. `media.commit { media_id }` → marks it live. Then reference the `media_id` in a message, post, avatar, etc.
- `media.favourite`, and gallery read over HTTP. Orphaned uploads are swept; live rows are re-verified against storage.

## Social feed — posts, likes, follows

One primitive powers many feed apps (each scoped by `app_id`; pick which account you post as with `identity.app_login`).

- Write: `feed.post`, `feed.delete`, `feed.like` / `feed.unlike`, `feed.comment`, `feed.follow` / `feed.unfollow`.
- Timeline read over HTTP (`GET /v1/feed?cursor`, `GET /v1/feed/posts/:id/comments`) — built fan-out-on-read.
- `sub feed:<app_id>` → advisory `feed.activity` ("something changed, go refetch"). Likes on *your* post arrive as a notification.

## Contacts & directory — numbers, blocks, listings

- **Contacts:** `directory.contacts.*` (CRUD). Contacts point at numbers, resolved to a character only at action time.
- **Resolve:** `directory.resolve { number }` → opaque routing id; never leaks the character behind a number.
- **Block:** `directory.block` / `directory.unblock`. Enforced at `channels.open_direct` and `calls.start` — gates *new* reach, never breaks an open thread, never tells the blocked party.
- **Listings (YellowPages/ads):** `directory.listings.*` with expiry.

## Notifications — reach a user whether or not they're online

Apps don't call notify directly — other primitives route through it. What you rely on:
if the user has a live socket, they get a `notify.event` push on `notify:<device_id>`;
if offline, it lands in an inbox they read on next login (`GET /v1/notify/inbox?cursor`).
Every notification has a class: `ring` (calls), `alert` (messages), `silent` (receipts,
likes). A muted channel downgrades to silent. Mark handled with `notify.seen { ids }` /
`notify.clear`. Badges are derived (`last_seq − last_read_seq`), never stored.

## Presence — online / last-seen, privately

`sub presence:<character_id>` for the people currently visible in your UI (thread header,
contacts) → `presence.state { online, last_seen_at? }`, snapshot on subscribe. A
character can hide both online dot and last-seen with `identity.set_share_presence { on }`.

## Settings — per device and per character

`identity.get_settings` / `identity.set_settings { scope: device|character, patch }`.
Opaque to core (wallpaper, ringtone, per-app toggles) — store whatever your client
needs, ≤16 KB per scope. `identity.me` returns your character, device, and app accounts.

## Admin — operator only, not app-facing

Tenant operators (not phone apps) manage tenants, rotate/delete API keys, freeze a
tenant, and read the audit log over the admin HTTP API. Freezing blocks new sessions
and money ops for that world.

---

## Cross-cutting — you get these for free

- **World isolation** — tenants can't see each other's data (enforced by convention *and* Postgres RLS).
- **Idempotency** — pass `client_uuid` on sends/transfers; retries are safe.
- **Ordering** — commands run one-at-a-time per connection, so per-user order is guaranteed.
- **Backpressure** — a too-slow client is dropped and resumes on reconnect; nothing durable is lost.
- **Errors** — every failure ack has a machine `code` (`unauthorized`, `forbidden`, `not_found`, `invalid`, `conflict`, `rate_limited`, `too_large`, `internal`). Key UI off `code`, not the message.
- **Rate limits** — token buckets per character; a trip returns `rate_limited { retry_after_ms }`, never a disconnect.
- **Pagination** — every list read returns `{ items, next_cursor }`; echo the opaque cursor back, don't parse it.

## Building an app? Map to primitives

- **Pure-web 1:1 chat + calls** → auth + channels + calls (1:1) — a browser client speaks the same WS protocol, no game involved.
- **WhatsApp-style messenger** → channels + media + notify + presence.
- **Instagram-style feed** → feed + media + directory.
- **Marketplace with escrow** → ledger (hold/capture) + directory listings + channels.
- **Group voice rooms (Discord-style)** → calls.group + presence + notify.
- **Crypto/wallet app** → ledger (+ exchange only if bridging external money).
