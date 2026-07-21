# Building a frontend on OPN

This package is deliberately not a UI. It is the wire layer — the part of an
OPN frontend that is the same no matter what you render with. Everything above
it (components, styling, state library, routing) is yours. Fork this, keep
`src/`, delete `example/`, and build.

What you get:

- `OpnSocket` — the single multiplexed WebSocket to Core: authentication,
  request/ack correlation, topic subscriptions with resume, reconnection,
  token refresh. Zero dependencies, no framework.
- `OpnHttp` — thin typed wrappers for the HTTPS read routes (history, media,
  feed pages, inbox).
- Types generated from Core's Rust contracts (`@opn/contracts`). You never
  hand-write a wire shape, and the compiler tells you when Core moves.

What you bring: a UI library (or none), a state store (or none), and — only if
you use group calls — the LiveKit JS SDK.

---

## 1. The mental model

OPN splits an in-game phone into two planes. The *game plane* (FiveM
client/server) does one data-plane job only: after a player picks a character,
the game server calls Core server-to-server and mints a **session token**,
which it hands to your UI. From that point your frontend is an ordinary web
app that happens to render inside GTA:

```
your UI  ── one WSS ──────────►  Core   (commands, events, presence)
your UI  ── HTTPS GET ────────►  Core   (history, media, feed pages)
your UI  ── WebRTC / LiveKit ─►  peers / SFU   (call media, never Core)
```

Three consequences worth internalizing:

1. **You never log in.** The browser receives a ready-made JWT. Your job is to
   pass it to `OpnSocket` and let the socket keep it fresh.
2. **The socket is the write path and the live path.** Every mutation is a
   command with an ack; every update you care about arrives as an event on a
   topic you subscribed to. HTTP is for cold, bulk reads only.
3. **Media bytes never touch Core.** Uploads go straight to S3 with presigned
   forms; call audio goes peer-to-peer or to a LiveKit SFU.

## 2. Getting started

```bash
npm install        # @opn/contracts is vendored in ./vendor/contracts, no other repo needed
npm test           # wire-layer unit tests
npm run example    # vanilla demo, see example/main.ts for token wiring
```

Minimal wiring:

```ts
import { createOpnClient, topics } from "@opn/client";

const { socket, http } = createOpnClient({
  url: "wss://core.example.com/ws",
  // Called on every (re)connect. Ask *your* backend / NUI bridge for a fresh
  // session JWT here — do not hardcode a token, they live for ~10 minutes.
  token: async () => (await fetch("/my-session-endpoint").then(r => r.json())).token,
});

await socket.connect();
const me = await socket.cmd("identity.me");
```

`createOpnClient` derives the HTTP base URL from the socket URL (`wss://x/ws`
→ `https://x`); pass `httpBaseUrl` if they differ.

### Where the token comes from

| Host | How the JWT reaches you |
|---|---|
| FiveM NUI | Game server calls `POST /v1/tenants/self/sessions` with its tenant API key, passes the token into the NUI via a message. Your `token` provider asks the Lua side for it. |
| Plain browser (dev, dashboards) | Any backend of yours that holds the tenant API key can mint one and serve it to the page. |

The tenant API key itself must never ship to a browser.

### Origins and CORS

Two separate gates, both server-side:

- **WebSocket:** Core checks the `Origin` header against the tenant's
  `allowed_origins` list. FiveM NUI origins (`https://cfx-nui-*`, `nui://`)
  are always allowed; a normal browser origin must be added to the tenant.
- **HTTP reads:** Core sets no CORS headers. Inside NUI this doesn't matter
  (CEF isn't enforcing cross-origin fetch the same way); from a normal
  browser origin, serve your app same-origin with Core behind a reverse
  proxy, or terminate both behind one host.

## 3. The socket lifecycle

You call `connect()`; the socket does the rest. What "the rest" is, precisely:

1. Fetch a token from your provider.
2. Open the WebSocket. The **first frame must be `auth` within 3 seconds** —
   the socket sends it for you on open.
3. On auth ack: state becomes `open`, queued commands flush, and every topic
   you subscribed to is re-subscribed with the last `seq` it saw.
4. Core pings every 30 s; the browser pongs automatically. Two missed pongs
   and Core closes the connection — you'll see a reconnect.
5. ~1 minute before the JWT expires the socket sends `auth.refresh` in-band
   and swaps the token. `OpnHttp` reads `socket.token`, so HTTP stays fresh
   too.

Commands issued while disconnected are queued and flushed after re-auth, so UI
code can mostly ignore connection state — but render it anyway (see §7).

### Close codes you should know

| Code | Meaning | Client behavior |
|---|---|---|
| 4401 | token rejected | re-fetch from your provider, retry once, then give up |
| 4408 | another connection took over this session | **no reconnect** — another tab/device owns the session now |
| 4409 | you consumed events too slowly | reconnect + resume |
| 1001 / 1006 | heartbeat loss / network drop | reconnect with backoff |

Takeover (4408) is deliberate last-writer-wins: opening the phone in a second
tab kills the first. Don't fight it in client code; surface "opened elsewhere"
in the UI.

## 4. Commands

One typed entry point:

```ts
const ack = await socket.cmd("channels.send", {
  channel_id: id,
  client_uuid: crypto.randomUUID(),
  body: { text: "hi", media_ids: null, gif_url: null, meta: null },
});
// ack: { message_id, seq }
```

Command names and payloads are the generated `Cmd` union — autocomplete will
show you the full surface (channels, directory, calls, ledger, feed, media,
notify, identity). Failures reject with `OpnError { code, msg }`, where `code`
is a wire `ErrCode` (`forbidden`, `not_found`, `rate_limited`, …) or a
client-side `closed`/`timeout`. Key UI copy off `code`; `msg` is for your
console.

`client_uuid` on sends and transfers is the idempotency key: keep it stable
across retries of the *same* user action and Core deduplicates.

Ack payloads are typed for the common commands (see `AckPayloads` in
`src/types.ts`); anything unlisted acks `unknown` — narrow it where you call.

## 5. Topics and events

Live data is topic-scoped. You subscribe, Core authorizes, and events arrive:

```ts
const unsub = socket.sub(topics.channel(channelId));

socket.on("channels.message", (msg, topic) => { /* all channels */ });
socket.onTopic(topics.channel(channelId), (evt, payload) => { /* one channel */ });
```

Topic kinds: `ch:<uuid>`, `call:<uuid>`, `notify:<device-uuid>`,
`presence:<character-uuid>`, `feed:<app-slug>`.

Rules the backend holds you to:

- **Snapshot on subscribe.** Most topics push current state right after the
  sub ack — you don't need a separate "get state" call for calls or presence.
- **Durable vs ephemeral.** Messages, receipts, call state are durable: if
  you consume too slowly Core closes the socket (4409) rather than drop them.
  Typing indicators and presence are ephemeral and may be dropped silently.
  Design accordingly: nothing critical in typing events.
- **Resume.** `sub` carries `last_seq`; the socket tracks the highest `seq`
  per topic automatically, so after a reconnect you get replay instead of a
  gap. Replay is capped (500): on `channels.resume_overflow` you must
  cold-load history over HTTP (§6) instead of trusting the stream.

`sub()` is reference-counted — two views subscribing to the same channel cost
one server subscription, and the last `unsub` releases it.

## 6. The message-list pattern

Every chat-like surface is the same three steps:

```ts
// 1. Subscribe first (so nothing falls between steps 1 and 2).
const unsub = socket.sub(topics.channel(id));
socket.on("channels.message", (m) => {
  if (m.channel_id !== id) return;
  insertBySeq(m);                       // 3. merge live events
});

// 2. Cold-load a page of history over HTTP, newest first.
const page = await http.channelMessages(id, { limit: 50 });
```

Merge by `seq` — it is a per-channel monotonic sequence, so dedup and ordering
are `seq` comparisons, no timestamp logic. Scrolling up is
`channelMessages(id, { beforeSeq: oldestLoaded })`.

Receipts: send `channels.mark_read`/`mark_delivered` with the highest seq
rendered; render other parties' watermarks from `channels.receipt` events and
the `last_read_seq` fields already present in `channels.list`.

## 7. State: bring your own

The socket is intentionally not a store. It gives you acks and event streams;
where that data lives is your call, and this is exactly the seam that keeps
the template UI-agnostic. Patterns that work:

**Vanilla / any framework** — module-level state plus re-render on event:

```ts
const channels = new Map<string, ChannelSummary>();
socket.on("channels.message", (m) => { bump(channels, m); render(); });
```

**React** — a hook per stream is enough; you don't need a state library to
start:

```ts
function useTopicEvents(topic: string) {
  useEffect(() => {
    const unsub = socket.sub(topic);
    const off = socket.onTopic(topic, onEvent);
    return () => { off(); unsub(); };
  }, [topic]);
}
```

**Zustand/Redux/pinia/svelte stores** — one bridge file that subscribes to
socket events and writes to the store; components never touch the socket.

Whatever you pick, render connection state. `socket.onState` gives you
`connecting | open | closed`; a thin "reconnecting…" banner on anything but
`open` is the difference between a solid-feeling phone and a haunted one.

## 8. Calls

### Group voice (SFU, LiveKit)

Core is the control plane only; the LiveKit JS SDK does the media. Install it
in *your app* (`npm i livekit-client`) — the wire client deliberately does not
depend on it.

```ts
import { Room } from "livekit-client";

const { call_id } = await socket.cmd("calls.group.create", { label: null, max_participants: null });
const unsub = socket.sub(topics.call(call_id));           // roster updates
const { sfu_url, token } = await socket.cmd("calls.group.join", { call_id });

const room = new Room();
await room.connect(sfu_url, token);                        // join within ~60 s, token is short-lived
await room.localParticipant.setMicrophoneEnabled(true);

socket.on("calls.group.state", (s) => renderRoster(s.participants));
// leaving: room.disconnect() + socket.cmd("calls.group.leave", { call_id })
```

Render the roster from `calls.group.state` (Core's truth, synced from LiveKit
webhooks), not from LiveKit participant events — that keeps your UI consistent
with what the rest of the system believes.

### 1:1 calls (P2P WebRTC)

`calls.start` / `accept` / `decline` / `hangup` drive the state machine;
`calls.state` snapshots include `ice_servers` (feed them to
`RTCPeerConnection`) and `calls.signal` is an opaque relay for your SDP/ICE
payloads. Core does not interpret them — the exchange format between the two
ends is yours to define.

## 9. Media upload

Bytes go to S3, not Core:

```ts
const ticket = await socket.cmd("media.request_upload", { kind: "image", bytes: file.size, mime: file.type });
for (const target of ticket.targets) {                 // presigned POST per role
  const form = new FormData();
  Object.entries(target.fields).forEach(([k, v]) => form.append(k, v));
  form.append("file", file);
  await fetch(target.url, { method: "POST", body: form });
}
await socket.cmd("media.commit", { media_id: ticket.media_id });
// now usable: channels.send with body.media_ids = [ticket.media_id]
```

Downloads: `http.media()` returns items whose `url`/`thumb_url` are
short-lived presigned GETs — treat them as ephemeral, re-fetch the list rather
than caching URLs.

## 10. Versioning

`http.healthz()` is unauthenticated and returns `contracts_version`. Check it
at boot against the version your `@opn/contracts` was generated from; a
mismatch on the major means the wire moved under you. Contracts follow semver
(`docs/contracts-semver.md` in opn-core) and additive changes are the norm.

## 11. Performance notes

The defaults are already the fast path — the work is in not breaking them:

- One socket per app. Never open a second (it takes over the session).
- Subscribe to what's on screen, unsubscribe on navigation (`sub()` returns
  its own cleanup; pair it with your framework's unmount hook).
- Consume events fast: handlers should update state and return, never await
  network. A slow consumer gets closed (4409) by design.
- Paginate history (`limit` caps at 100 server-side); render from the live
  stream after the first page instead of re-fetching.
- Batch UI updates per animation frame if a channel is hot; the socket
  delivers events synchronously in arrival order.
