# OPN — Group Voice Calls (draft v0.1)

Companion to [OPN-CORE.md](OPN-CORE.md) §10.4 (calls). Same reading rules:
sprints are scope-bound, not time-bound; each has Goal / Depends on / Work
items / Test plan / Exit criteria. Core-side contract changes below go
through a CDR in OPN-CORE.md before implementation.

**Status (2026-07-21): G0, G1, and G3 shipped** (contracts, LiveKit service,
group-call primitive, token mint, webhook sync; group rate limits, per-tenant
concurrent-room cap, webhook-signature-failure alert, runbook, kill-LiveKit
chaos drill). The 24 h soak is deferred by decision (run on the perf host when a
release is cut; the `soak10x` baseline already gates data-plane p99). Only G2
(client + web demo) remains — it targets `opn-web-conference` / `@opn/client`,
now archived, so it is out of scope until the web client returns.

## Why this doc exists

Sprint 6 delivered 1:1 calls as an **opaque signaling relay**: Core forwards
SDP/ICE verbatim, media flows peer-to-peer (DTLS-SRTP), Core never terminates
media. That design caps out at mesh topologies — fine for 1:1, workable for
voice up to ~8–10 peers, dead for anything larger (per-peer upload and
encode grow linearly).

Requirement: **group voice calls with more than 10 participants.** That
forces an SFU. This doc defines how to add one without giving up the
property that made Sprint 6 cheap to operate.

## Architecture decision: SFU as sidecar, Core stays control plane

**LiveKit (self-hosted, single binary)** runs next to Core. Core never
proxies media. The split:

| Concern | Owner |
|---|---|
| Group membership, invite/join/leave FSM, persistence | opn-core |
| Access control (who may join which room) | opn-core (mints LiveKit access token) |
| Media transport, forwarding, simulcast | LiveKit |
| Truth-sync (participant joined/left, room closed) | LiveKit webhooks → opn-core |

Client flow: `calls.group.join` → Core checks membership → ack carries a
short-lived LiveKit access token + LiveKit URL → client connects **directly**
to LiveKit. Media bytes never pass through the Core process.

Why LiveKit over mediasoup/Janus: single static binary, first-class JS SDK,
JWT room-token model that matches Core's existing minting pattern, webhook
API for state sync. Voice-only forwarding is packet relay (no transcode) —
10–20 voice participants is a light load even on the current prod host.

### What this deliberately gives up

1:1 calls are E2E-private (DTLS-SRTP terminates only on peers). **Group
calls are not**: LiveKit terminates media encryption on our infra. This is
inherent to any SFU. It must be documented in the product spec and surfaced
to template forkers. 1:1 calls stay on the existing P2P path unchanged —
group is a new primitive, not a rewrite.

### Contracts shape (CDR required)

- New commands: `calls.group.create`, `calls.group.join`, `calls.group.leave`,
  `calls.group.end` (creator/privileged only).
- New event: `calls.group.state` — full snapshot per change, same convention
  as `calls.state`.
- `topology` field (`"p2p" | "sfu"`) on call snapshots from day one, so a
  future topology change never breaks pinned clients (additive-only semver,
  [contracts-semver.md](contracts-semver.md)).
- Join ack payload: `{ sfu_url, token, expires_at }`. Token TTL short
  (≤60 s single-use window to connect; LiveKit session survives token expiry).

### Persistence

Reuse `call_sessions` (add `topology` column) and `call_participants` — the
Sprint 6 schema is already N-participant shaped. Add `sfu_room_id` to
`call_sessions`. Janitor extends to reap rooms LiveKit reports empty.

### Deploy

New service in the compose files: `livekit` with pinned image, UDP port
range published, API key/secret shared with Core via env. LiveKit's
embedded TURN stays off — existing coturn config covers ICE for 1:1;
LiveKit clients reach it directly over UDP/TCP fallback.

## Sprint sequence at a glance

| # | Name | Depends on | Delivers |
|---|---|---|---|
| G0 | Contracts + infra | Core Sprint 6 | **done** — CDR merged, contracts types, livekit service in dev compose, health checked |
| G1 | Group-call primitive | G0 | **done** — FSM + store + token mint + webhook sync, HTTP active-calls includes groups |
| G2 | Client + proof | G1 | **out of scope** — `@opn/client`/`opn-web-conference` archived; revisit when the web client returns |
| G3 | Hardening | G1 | **done** — group rate limits, per-tenant room cap, webhook-sig alert, runbook, kill-LiveKit chaos drill; 24 h soak deferred to release |

## Sprint G2 — Client + proof

**Goal**: `@opn/client` speaks group calls; a human can hear it work.
**Depends on**: G1.

Work items: group-call methods + events in `@opn/client` (still React-free;
LiveKit JS SDK is a peer dependency of the app, not the wire client);
minimal group-voice room in `opn-web-conference` (POC status unchanged —
proof, not product).

Test plan: three browser tabs in one group call, join/leave reflected in
snapshots.
Exit: 3-way audio demo on dev stack, `npm run dev` only.

## Sprint G3 — Hardening

**Goal**: safe to run on the prod host without watching it.
**Depends on**: G2.

Work items — code (shipped): rate limits on group commands (Social class,
`infra/ratelimit.rs::class_of`); max concurrent rooms per tenant
(`LIVEKIT_MAX_ROOMS`, default 50; `group::rooms_admit` enforced in
`store::group_create`, RLS-scoped count → `Conflict` at the cap); runbook
`runbooks/livekit-degraded.md`; alert on webhook signature failures
(`opn_livekit_webhook_total{outcome="rejected"}` → `LivekitWebhookRejected` in
`deploy/prometheus/alerts.yml`).

Work items — kill-LiveKit chaos (shipped): `chaos/livekit-down.sh` +
`opn-loadgen --group-probe`. The fail-closed seam is config, not the running
SFU: Core mints the access token locally and never calls the LiveKit server
synchronously, so a *down* SFU degrades only client **media** — Core group
commands fail closed only when LiveKit is *unconfigured* (`state.cfg.livekit ==
None` → every `calls.group.*` answers `forbidden`, unit-covered). The drill
therefore asserts the contrapositive of "degrades only group media": with the
SFU SIGKILLed, the group **control plane** still acks create/join and mints a
token (`--group-probe`), 1:1 calls + the /link relay still run (`--link-drop`),
and Core stays healthy — i.e. everything that is not group media survives.

Work items — 24 h soak (deferred by decision): calls churning vs the 16 ms
baseline, on the perf host at release time. The existing `soak10x` run already
gates data-plane p99; the group-call soak is additive and not on the critical
path for this workstation.

Exit: kill-LiveKit chaos degrades only group-call media (control plane, 1:1, and
data plane hold) — met by `chaos/livekit-down.sh`. Soak: deferred.

## Explicit non-goals (v1)

Group **video** (voice only — video multiplies SFU load and belongs to its
own capacity discussion); E2EE for group calls (insertable streams — gated,
revisit on demand); recording; PSTN; screen share; >1 SFU node.
