# OPN-CORE

## Quickstart

Prerequisites: [Rust](https://rustup.rs), [Docker](https://docs.docker.com/engine/install/), [just](https://github.com/casey/just).

```sh
cp .env.example .env
just dev
cargo run -p opn-core --bin opn-core
curl localhost:8080/healthz   # -> 200
```

## Mint a session

Create a tenant (owner-role CLI; the API key is printed once, only its hash
is stored):

```sh
cargo run -p opn-core --bin opn-core -- admin create-tenant --name demo --new-world demoworld
```

Then mint a session with the printed key:

```sh
curl -X POST localhost:8080/v1/tenants/self/sessions \
  -H "Authorization: Bearer <api key>" \
  -H "Content-Type: application/json" \
  -d '{"framework_ref":"steam:110000112345678"}'
# -> { token, session_id, character: { number: "555-XXXX", ... }, device }
```

## Load testing

`opn-loadgen` seeds a population over the real mint API, drives N paired
WebSocket connections at a target rate, and self-asserts on the result. The
committed `smoke` scenario (JSON, not TOML) is 300 conns / 30 msg/s / 300s and
gates on **ack p99 < 25 ms** and **zero durable (4409) closes**; a breach exits
non-zero. It runs nightly in the `Perf smoke` workflow.

Locally, with the stack and server up (see Quickstart), mint a tenant and pass
its key to loadgen:

```sh
key=$(cargo run -p opn-core --bin opn-core -- \
  admin create-tenant --name loadgen --new-world loadgenworld \
  | sed -n 's/^api key:[[:space:]]*//p')
OPN_LOADGEN_API_KEY=$key \
  cargo run --release -p opn-loadgen -- --scenario crates/loadgen/scenarios/smoke.json
```

Release is required — a debug build's latency alone would breach the p99 gate.
The one-line JSON summary goes to stdout; the human table goes to stderr.

Load tests connect from a single IP, so boot the server with
`OPN_PREAUTH_PER_IP_MAX` raised above the connection count (default is 5) —
otherwise the gateway 429s all but 5 of the connections.

## Tenant link (voice targets)

The FXServer gateway resource opens one WebSocket per world to
`ws://<core>/link`, authenticated by the tenant **API key** in the
`Authorization: Bearer <key>` header (a native client, not a browser — headers
are fine). Its first frame is a hello:

```json
{ "resource_version": "1.2.3", "contracts_version": "0.1.0" }
```

Core acks `{ "reply_to": 0, "ok": true }`, logs the version pair, and then
pushes **down-only** `calls.voice` events (`set_targets` on call accept, `clear`
on end) — the resource binds/unbinds game-voice accordingly. Nothing flows up
the link; all FXServer→Core calls are plain HTTPS with the API key. On
(re)connect the resource re-syncs open calls:

```sh
curl localhost:8080/v1/tenants/self/calls/active -H "Authorization: Bearer <api key>"
# -> [ { call_id, kind, state, participants: [...] }, ... ]
```

WebRTC ICE (STUN/TURN) is static config — Core only echoes `OPN_ICE_SERVERS`
into every `calls.state` snapshot; video bytes go P2P or via coturn (the dev
`coturn` compose service), never through Core. Set `OPN_ICE_SERVERS` (see
`.env.example`) to point clients at your relay.

See [../docs/OPN-CORE.md](../docs/OPN-CORE.md) for architecture and [../docs/opn-core-roadmap.md](../docs/opn-core-roadmap.md) for the roadmap.
