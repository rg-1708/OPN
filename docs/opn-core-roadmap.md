# OPN-CORE — Implementation Roadmap & Sprint Plan (v1)

Companion to [OPN-CORE.md](../OPN-CORE.md) (the design) and [OPN.md](../OPN.md)
(the system). This document is the *build order*: every sprint, what goes in
it, the algorithms and structures a developer must use, how it is tested, and
what "done" means. Where the design doc already decides something, this doc
cites the section instead of restating it — **the design doc wins on any
conflict**; if implementation reveals the design is wrong, amend OPN-CORE.md
(new CDR) first, then code.

Guiding constraint (ADR-1 amendment): stability + performance over shipping
velocity. Sprints are **scope-bound, not time-bound**. A sprint is done when
its exit criteria pass, not when two weeks elapse. The nominal durations below
assume one full-time developer and exist only for planning; slipping a sprint
is fine, shipping a sprint without its tests is not.

---

## 0. How to read this document

Each sprint has five parts:

- **Goal** — the one-sentence outcome.
- **Depends on** — sprints that must be complete first. The order below is a
  valid topological order; parallelize only where the graph allows and only
  if there are multiple developers.
- **Work items** — the concrete build list, with algorithm/architecture
  choices spelled out. Where a choice is left open in OPN-CORE.md, it is
  closed here.
- **Test plan** — what must exist in `cargo test` / CI by sprint end. Tests
  ship in the same sprint as the code they cover, never "next sprint".
- **Exit criteria** — the checklist that gates the sprint. All items binary,
  all verifiable by running something.

### Cross-cutting rules (apply to every sprint)

1. **Contracts drift gate** (OPN-CORE.md §15): CI regenerates `.d.ts` from
   the `contracts` crate and fails on uncommitted diff. Active from Sprint 0
   onward; never disabled.
2. **Command coverage is compiler-enforced.** A single `#[test]` in the
   protocol harness contains an exhaustive `match` over the `Cmd` enum where
   each arm names the integration test function covering that command.
   Adding a `Cmd` variant without a test breaks the build. Same pattern for
   `Evt`. This is how "we cover the API" stays true without discipline.
3. **HTTP route coverage**: every route in the axum router has at least one
   integration test hitting the real router (`tower::ServiceExt::oneshot`
   or a bound listener). Tracked by a checklist test that lists routes and
   asserts against the router's registered paths.
4. **RLS-on in tests** (CDR-3): the test database role is the non-bypassing
   app role from Sprint 0 on. Tests that pass only as superuser are bugs.
5. **Perf smoke in CI** from Sprint 4 onward: nightly job runs `opn-loadgen`
   (built in Sprint 4, extended later) for 5 minutes at design load
   (300 conns, 30 msg/s) against a compose stack; fails on p99 command
   latency > 25 ms or any durable-queue disconnect. Loose thresholds on
   purpose — it catches regressions in kind, not degree. Tight thresholds
   live in Sprint 10.
6. **No `unwrap`/`expect` on request paths.** `unwrap` is allowed only in
   `main` startup (fail-fast config) and tests. Enforced by
   `clippy::unwrap_used` at deny level for the `core` crate, with
   `#[allow]` at the two permitted sites.
7. **Every janitor task is idempotent and crash-safe**: it must be correct
   to run twice concurrently (advisory lock per task name via
   `pg_advisory_xact_lock(hashtext('janitor:<name>'))`) and correct to have
   died mid-run.
8. Migrations are forward-only plain SQL (`sqlx migrate`), each domain-table
   migration includes its RLS policy in the same file.

### Sprint sequence at a glance

| # | Name | Nominal | Depends on | Delivers |
|---|---|---|---|---|
| 0 | Foundations | 2 w | — | workspace, contracts skeleton, config, CI, compose stack, health/metrics, RLS groundwork |
| 1 | Identity & auth | 2 w | 0 | tenants/worlds/characters/devices/sessions, API keys, JWT, session mint, number assignment, settings |
| 2 | WS gateway | 2 w | 1 | connection lifecycle, registry, sub/unsub, backpressure, heartbeat, dispatch, rate limiting, presence, janitor |
| 3 | Notify + channels hot path | 2 w | 2 | inbox + routing, channel schema, send path (seq, idempotency, fan-out), open_direct/create/list, protocol harness |
| 4 | Channels complete + pagination + loadgen v0 | 2 w | 3 | receipts, typing, reactions, pins, members, resume, history HTTP, cursor idiom, `opn-loadgen` |
| 5 | Media + directory | 2 w | 4 | presigned uploads, commit, janitor verification, gallery; contacts, blocks, resolve, listings |
| 6 | Calls + tenant link | 2 w | 5 | call FSM, signaling relay, `/link` gateway, voice-target events, re-sync |
| 7 | Ledger + exchange | 2 w | 3 | accounts, transfers, holds, escrow, exchange protocol, reconciliation |
| 8 | Feed | 1–2 w | 4 | posts/follows/likes/comments, fan-out-on-read timeline, advisory events |
| 9 | Verification hardening | 2 w | 3–8 | property tests, protocol fuzzing, chaos drill scripts, RLS audit |
| 10 | Performance & soak | 2 w | 9 | full load scenarios, profiling, bottleneck fixes, 24 h soak, tight perf gates |
| 11 | Release engineering | 1–2 w | 10 | Coolify deploy, partition automation, backups, alerts, runbooks, contracts publish, v1.0 |

Total nominal: ~24 weeks. Sprints 5–8 touch disjoint primitives and can
partially overlap with a second developer; 0–4 and 9–11 are strictly serial.

Why this order and not primitive-by-priority: each sprint hardens what the
next depends on. Channels is the hot path and the most subtle primitive, so
it lands earliest after the gateway and therefore *soaks longest* under the
perf smoke before release (mirrors the app build order argument in
OPN.md §14.5). Ledger needs only the gateway + notify, but it lands after
calls-adjacent work because nothing in the MVP app cut consumes it on day
one, while Dialer needs calls. Feed is post-MVP surface (no v1 app) and sits
last among primitives.

---

## Sprint 0 — Foundations

**Goal**: a running binary with health checks, an empty-but-real contracts
pipeline, CI that enforces every cross-cutting rule, and a dev stack any
contributor brings up with one command.

**Depends on**: nothing.

### Work items

1. **Cargo workspace** exactly per OPN-CORE.md §2: `crates/contracts`,
   `crates/core`. Add `crates/loadgen` as an empty placeholder member (built
   in Sprint 4) so workspace-level lints and CI config are set once.
   Toolchain pinned via `rust-toolchain.toml` (stable, exact version).
2. **Contracts crate v0**:
   - `envelope.rs`: `ClientFrame { id: u64, /* flattened */ cmd: Cmd }` on
     the wire as `{ id, cmd, payload }` via
     `#[serde(tag = "cmd", content = "payload", rename_all = "snake_case")]`
     on `Cmd` (OPN-CORE.md §7). Server side: `ServerMsg` enum with two
     variants — `Ack { reply_to: u64, ok: bool, payload: Option<serde_json::Value → typed later>, err: Option<Err> }`
     and `Push { evt: Evt, topic: String }`. Do **not** use
     `serde_json::Value` for typed payloads beyond the first commit —
     each ack payload gets a concrete type as commands land.
   - `error.rs`: the eight-variant `ErrCode` enum + `Err { code, msg }`
     (§7). This enum is closed; adding variants is a contracts-major event.
   - `cmd.rs` / `evt.rs`: start with `Sub`, `Unsub`, `AuthRefresh` variants
     only (handlers come in Sprint 2); the enums exist so the drift gate and
     coverage-match tests exist from day one.
   - **Delivery class**: `Evt` gets a method `fn class(&self) -> EvtClass`
     (`Durable | Ephemeral`) — an exhaustive match in `contracts`, so every
     new event is forced to declare its backpressure class (§4.3) at the
     type level.
   - `bin/export_ts.rs` using `ts-rs`: writes `bindings/*.d.ts`, committed.
     CI job: regenerate + `git diff --exit-code`.
3. **Config** (`config.rs`, §13): one `Config` struct, `Config::from_env()`,
   every field required unless it has a documented default, first missing
   var aborts startup with the var name. No config files, no reload.
4. **State**: `AppState { pg: PgPool, redis: redis::aio::ConnectionManager, registry: SessionRegistry (stub), limits: RateLimitTable (stub), cfg: Arc<Config> }`,
   `Arc`-cloned. Pool: `max_connections = 20`,
   `acquire_timeout = 3 s` — an exhausted pool must fail fast into an
   `internal` ack, not queue forever.
5. **HTTP skeleton** (axum): `GET /healthz` (PG `SELECT 1` + Redis `PING`,
   both with 1 s timeout; 503 on failure), `GET /metrics` on the separate
   `OPN_METRICS_BIND` listener (Prometheus text via `metrics` +
   `metrics-exporter-prometheus`). Two `axum::serve` calls, one per bind.
6. **Observability plumbing** (§14): `tracing` + `tracing-subscriber` JSON
   to stdout; `RUST_LOG` respected. Define the metric names now (even at
   zero) so dashboards never chase renames: `opn_connections`,
   `opn_commands_total{cmd,outcome}`, `opn_command_seconds{cmd}` (histogram),
   `opn_sendq_depth`, `opn_sendq_drops_total{class}`, `opn_pg_pool_in_use`,
   `opn_inbox_inserts_total`, `opn_janitor_runs_total{task,outcome}`.
7. **IDs** (`infra/ids.rs`): `uuid` crate with `v7` feature;
   `pub fn new_id() -> Uuid` — the only way any code mints an id.
8. **Migrations runner**: `sqlx::migrate!` at startup (single-replica rule,
   §9). Migration `0001` creates the two DB roles and RLS groundwork:
   - `opn_migrate` (owner, runs migrations, implicitly bypasses RLS on
     owned tables), `opn_app` (`NOSUPERUSER`, no `BYPASSRLS`) — the
     runtime `DATABASE_URL` user.
   - Helper for later migrations documented in-repo: every domain table gets
     `ALTER TABLE t ENABLE ROW LEVEL SECURITY; ALTER TABLE t FORCE ROW LEVEL SECURITY;`
     plus policy
     `USING (world_id = current_setting('app.world_id')::uuid)`.
   - `infra` helper `pub async fn world_tx(pool, world_id) -> Transaction`:
     `BEGIN` + `SET LOCAL app.world_id = $1` (via `set_config($1,$2,true)`
     to stay parameterized). **All domain-table access goes through a
     transaction opened by this helper** — this is a hard convention;
     reviewers reject any `pool`-direct domain query.
9. **Dev stack**: `docker-compose.dev.yml` — Postgres 16, Redis 7, MinIO
   (+ one-shot `mc` container creating the bucket). `just dev` /
   `just test` recipes (or a `Makefile`; pick one, never both).
10. **CI** (GitHub Actions or equivalent): `cargo fmt --check`,
    `cargo clippy --all-targets -- -D warnings` (with rule 6's lint),
    `cargo test` against compose services, `cargo sqlx prepare --check`,
    contracts drift job. Cache the target dir; Rust CI cost is real, pay it
    once per push not per job.

### Test plan

- `/healthz` integration test: 200 with services up; 503 when PG container
  stopped (testcontainers or compose-managed).
- Config test: missing var → startup error naming the var.
- RLS canary test: as `opn_app`, a query without `SET LOCAL app.world_id`
  against a seeded dummy domain table returns zero rows; with it, returns
  the seeded row. This test is the pattern-proof the whole isolation story
  rests on — write it first.
- Contracts round-trip test: `ClientFrame` with each existing `Cmd` variant
  serializes to the documented wire shape (golden JSON strings in the test,
  not re-derived).

### Exit criteria

- [ ] `docker compose -f docker-compose.dev.yml up` + `cargo run` yields a
      200 `/healthz` from a clean checkout, documented in `README`.
- [ ] CI green with all five jobs; drift gate demonstrably fails on an
      uncommitted contracts change (test it once, keep the run link).
- [ ] RLS canary test passes as `opn_app`.
- [ ] Metric names above visible on `/metrics`.

---

## Sprint 1 — Identity & auth

**Goal**: a tenant can mint a session over HTTP with its API key; the session
resolves to a character/device with a phone number; JWTs verify; settings
read/write. Everything the gateway needs to authenticate a socket.

**Depends on**: Sprint 0.

### Work items

1. **Schema** (§10.1): `worlds`, `tenants`, `characters`, `devices`,
   `app_accounts`, `sessions`, `retired_numbers` — each with RLS policy in
   the same migration (exception: `tenants` and `worlds` are not
   world-scoped domain rows; they are accessed only by infra code paths and
   get **no** `opn_app` grants beyond the specific columns infra needs).
   Constraints from the design doc verbatim: unique
   `(world_id, framework_ref)`, partial unique
   `(world_id, number) WHERE number IS NOT NULL`, unique
   `(world_id, app_id, handle)`. Add `sessions` index on `(expires_at)`
   for the janitor.
2. **API-key auth** (§11): key format `opn_<43 chars base64url>` (256-bit
   random). Store `sha256(key)` in `tenants.api_key_hash`; auth = hash the
   presented key, indexed lookup by hash. High-entropy key → no KDF, no
   constant-time comparison needed (the lookup key *is* the secret's hash).
   Axum extractor `TenantAuth` produces `tenant_id + world_id`; used by all
   `/v1/tenants/self/*` routes. Key creation is a CLI subcommand
   (`opn-core admin create-tenant`), key printed once, never stored.
3. **Session mint** `POST /v1/tenants/self/sessions` (§6, OPN.md §3):
   payload `{ framework_ref, device_id? }`.
   - Upsert character by `(world_id, framework_ref)`
     (`INSERT … ON CONFLICT DO UPDATE SET last_seen_at = now()` returning
     the row).
   - **Number assignment algorithm** (first sight only, i.e. number IS
     NULL): generate candidate from the world's pattern (config default
     `555-XXXX`, digits uniform random), then in one statement attempt
     `UPDATE characters SET number = $cand WHERE id = $id AND number IS NULL
     AND NOT EXISTS (SELECT 1 FROM retired_numbers WHERE world_id = $w AND
     number = $cand AND freed_at > now() - interval '30 days')` — the
     partial unique index catches concurrent duplicates; on unique-violation
     or zero-row update, retry with a fresh candidate, max 10 attempts,
     then `internal` (a full number space is an operator problem, not a
     retry problem — log loudly).
   - Insert `sessions` row (TTL from `OPN_SESSION_TTL_SECS`), mint JWT,
     return `{ token, session_id, character: {...}, device: {...} }`.
4. **JWT** (`infra/auth.rs`, §11): `jsonwebtoken`, HS256, claims
   `{ sid, tenant, world, char, device, exp }`, 10 min. `verify()` checks
   signature + `exp`, then `sessions.revoked_at IS NULL AND expires_at >
   now()` (one indexed read). Returns the typed `Identity` struct that
   becomes the dispatch `ctx` — **the only constructor for `Identity` is
   this verify path**; handlers cannot fabricate one (enforces "never read
   identity from payload", §7, at the type level).
5. **Settings** (§10.1): `identity.get_settings` / `set_settings
   { scope: device|character, patch }` — stored as whole-document replace
   (not JSON merge patch; the client owns the document, Core validates only
   `pg_column_size ≤ 16 KB` → `too_large`). `characters.share_presence` is
   a real column, settable only via a dedicated
   `identity.set_share_presence` command (it gates server behavior; keep it
   out of the opaque blob).
   These are WS commands — implement the handlers now as plain
   `pub async fn`s in `primitives/identity`, wire to dispatch in Sprint 2.
6. **`identity.me`**, **`identity.app_login`**: same pattern —
   `app_login` validates the `app_account` belongs to ctx character, then
   updates the *session row's* active-account map (per OPN.md §3: per
   session, not per app code). Add `sessions.app_accounts jsonb` for it.
7. **Per-tenant config cache** (§13): `moka` or hand-rolled
   `RwLock<HashMap<TenantId, (Arc<TenantCfg>, Instant)>>` with 60 s TTL —
   hand-rolled is ~30 lines and one less dependency; do that.
8. **Janitor v0** (§3): the 30 s tick loop skeleton with task registry;
   first tasks: delete sessions where `expires_at < now() - 7 days`
   (keep recent-expired for audit), sweep `retired_numbers` older than
   cooldown. Each task: advisory lock, own tracing span, failure increments
   `opn_janitor_runs_total{outcome="err"}` and never kills the loop.

### Test plan

- `#[sqlx::test]` per store fn: character upsert idempotency, number
  uniqueness under 32 concurrent mints in one world (spawn tasks, assert
  all numbers distinct), cooldown exclusion (seed `retired_numbers`, assert
  candidate rejected), session revocation honored by `verify()`.
- HTTP tests: mint happy path; wrong API key → 401; unknown
  `framework_ref` creates character; second mint reuses number.
- JWT: expired token rejected; tampered signature rejected; revoked session
  rejected.

### Exit criteria

- [ ] `curl` from README mints a real session against the dev stack.
- [ ] Concurrent-mint number test green (this is the sprint's subtle bug
      magnet; it must exist and pass 100 consecutive runs —
      `cargo test -- --test-threads` doesn't matter, the test itself spawns
      its concurrency).
- [ ] All new tables have RLS policies; canary-style test per table proves
      cross-world reads return empty.

---

## Sprint 2 — WS gateway

**Goal**: an authenticated, heartbeated, rate-limited WebSocket with
subscriptions, presence, backpressure, and sequential dispatch — the chassis
every primitive plugs into.

**Depends on**: Sprint 1.

### Work items

1. **Connection lifecycle** (§4.1) exactly as specified:
   - `GET /ws` upgrade with `Origin` validation against tenant
     `allowed_origins` + `cfx-nui-*` (reject → 403 pre-upgrade; tenant is
     unknown pre-auth, so pre-upgrade check is against the *union* of all
     cached origins, and the authoritative per-tenant check re-runs after
     the auth frame — document this two-phase check in code).
   - First frame must be `auth` within 3 s (`tokio::time::timeout` on the
     first read): else close `4401`; any other first frame `4400`.
   - Pre-auth caps: global `AtomicU32` (config, default 1000) and per-IP
     `DashMap<IpAddr, u8>` (default 5), decremented on auth/close. Over
     cap → close immediately, no handshake work.
   - On auth success: register, ack, spawn reader + writer.
   - **Session takeover** (last-writer-wins): `registry.register` returns
     the previous `ConnHandle` if any; send it close code `4408` and abort
     its tasks. The new connection wins unconditionally.
2. **SessionRegistry** (§4.2): start with `DashMap` as the doc says:
   `sessions: DashMap<SessionId, ConnHandle>`,
   `topics: DashMap<(WorldId, Topic), SmallVec<[SessionId; 4]>>` where
   `Topic` is an interned `Arc<str>`. `ConnHandle { tx: mpsc::Sender<Outbound>, identity: Identity, subs: Mutex<HashSet<Topic>>, abort: AbortHandle }`.
   Unregister removes the session from every topic in its `subs` set —
   `subs` exists precisely so disconnect is O(own subs), not a full topic
   scan.
3. **Writer task + backpressure** (§4.3): bounded `mpsc` capacity 256.
   Publish uses `try_send`:
   - durable event, queue full → close the connection (code `4409`
     "slow consumer"), increment `opn_sendq_drops_total{class="durable_close"}`.
   - ephemeral event, `tx.capacity() < 52` (≈20 % headroom left, i.e.
     ≥80 % full) → drop silently, count it.
   The class comes from `Evt::class()` (Sprint 0) — no per-call-site
   judgment calls.
4. **Heartbeat**: writer sends WS ping every 30 s; reader records pong
   time; writer closes after 2 missed (i.e. no pong for >60 s). Client
   pings are answered by the WS library automatically.
5. **Dispatch loop** (§7, CDR-5): sequential per connection —
   read frame → parse `ClientFrame` (parse error → `invalid` ack with
   `reply_to: 0` if id unknown, then continue; never close on bad JSON —
   fuzzing in Sprint 9 assumes this) → rate limit → `match cmd` →
   `handler(ctx, payload).await` → ack. Wrap every handler call in a span
   (`cmd`, `tenant`, `world`, `char`, `duration`, `outcome`) and the
   latency histogram. A handler returning `Err(anyhow)` logs at `error`
   and acks `internal` with no detail (§7).
6. **Rate limiting** (`infra/ratelimit.rs`, §12): classic lazy token
   bucket, no background refill:
   ```rust
   struct Bucket { tokens: f64, last: Instant }
   // on check(class): refill = elapsed * rate; tokens = min(burst, tokens + refill)
   // tokens >= 1.0 → take 1, Ok; else Err(retry_after_ms = ((1.0 - tokens) / rate * 1000) as u64)
   ```
   Table: `DashMap<(CharacterId, Class), Mutex<Bucket>>`, lazily inserted;
   janitor sweeps entries idle > 10 min. Class-per-command mapping is an
   exhaustive `fn class(cmd: &Cmd) -> Class` in dispatch — compiler forces
   every new command to pick a bucket. Exceeded → `rate_limited` ack with
   `retry_after_ms`, never disconnect.
7. **`sub`/`unsub`** (§4.2, §4.4): topic string parsed into a typed
   `TopicKind` enum (`Ch(Uuid) | Feed(AppId) | Call(Uuid) | Notify(DeviceId) | Presence(CharacterId)`)
   — unknown shape → `invalid`. Authorization delegates to the owning
   primitive via a plain match (no trait): `channels::authorize_sub`,
   `calls::authorize_sub`, etc. For this sprint only `notify` (own device
   only) and `presence` are implementable; others land with their
   primitives and until then return `not_found`. Snapshot-on-sub semantics
   (presence, later calls): the authorizing primitive returns
   `Option<Evt>` pushed to the subscriber immediately after registration,
   *before* the sub ack — so the client can treat "ack received" as
   "snapshot delivered".
8. **Presence** (§4.2, CDR-6): Redis `SET presence:<world>:<char> 1 EX 90`
   refreshed by the gateway every 30 s per connection (piggyback the
   heartbeat tick, one pipelined `MSET`-style pass over local sessions,
   not per-conn round trips); `DEL` + `characters.last_seen_at = now()`
   on disconnect. Sub authorization checks `share_presence`: off →
   snapshot `{ online: null }`, and the publisher never emits for that
   character (checked at emit time against a 60 s-cached flag).
   Presence transitions publish to `presence:<char>` subscribers:
   on connect (`online: true`), on disconnect (`online: false,
   last_seen_at`). Single-replica: transitions are local registry events;
   the Redis key exists so replica 2+ and `/healthz`-style introspection
   read the same truth later.
9. **`auth.refresh`** (§11): over live WS; re-checks `revoked_at`, bumps
   `sessions.expires_at`, returns fresh JWT. Registry kill on revocation:
   revoke path (admin, later) looks up the registry and closes live
   sessions — wire the lookup now.
10. **Redis pub/sub listener** (§3, §8): behind `OPN_REPLICAS > 1` — a
    task subscribing `opn:*` patterns; received payloads deserialize to
    `Evt` + topic and re-publish locally. Build it now (it is ~80 lines),
    test it with two in-process gateway instances sharing one Redis —
    waiting until "we add a replica" means it ships untested.

### Test plan

- Protocol harness v0 (`tests/ws.rs`): a test client helper
  (`tokio-tungstenite`) with `connect_and_auth()`, `cmd()`, `expect_evt()`
  — this helper is the backbone of every primitive's tests from here on;
  invest in ergonomics now.
- Lifecycle: no-auth-in-3s → 4401; garbage first frame → 4400; bad JWT →
  4401; takeover kills old socket (old receives 4408); missed pongs →
  close.
- Backpressure: fill a connection's queue with a stalled reader, assert
  durable publish closes it and ephemeral publish drops (needs a test-only
  low capacity — make queue capacity a config with prod default 256).
- Rate limit: burst then sustained; `retry_after_ms` sane; other class
  unaffected.
- Presence: sub own char → snapshot; toggle `share_presence` off → null
  snapshot, no events on disconnect.
- Two-instance Redis fan-out test as described in item 10.

### Exit criteria

- [ ] Harness helper merged and used by every gateway test.
- [ ] `Cmd` coverage match-test exists (rule 2) listing `sub`, `unsub`,
      `auth.refresh`, identity commands — and fails when a variant is added
      without a test.
- [ ] 300 idle authenticated connections hold steady-state RSS growth ≈ 0
      over 10 min (manual check this sprint; automated in Sprint 10).

---

## Sprint 3 — Notify + channels hot path

**Goal**: the product's spine — a message sent is persisted, sequenced,
acked, fanned out live, and inboxed offline. After this sprint the system
does its job end to end.

**Depends on**: Sprint 2. Notify comes first inside the sprint (channels
routes through it).

> **Amendment (2026-07-18, build):** item 2's "all five tables" shrank to
> three — `channels`, `channel_members`, `messages` (partitioned). `reactions`
> and `channel_pins` moved to Sprint 4 with their handlers. Rationale: the
> "do it now" argument is retrofit-is-a-rewrite, which applies only to
> `messages` *partitioning*; the two unpartitioned tables have no retrofit cost
> and no Sprint 3 consumer or test, so front-loading them is pure YAGNI (the
> "scope may shrink by moving items later" allowance). See reflections
> 2026-07-18 (Sprint 3), decision 5.

### Work items

1. **Notify** (§10.8, CDR-1):
   - `inbox` table + RLS.
   - `pub async fn route(tx_or_pool, recipient: CharacterId, n: Notification)`:
     look up recipient's live sessions in the registry (any device) →
     push `notify.event` on `notify:<device_id>`; none → insert inbox row.
     `Notification { app_id, kind, class: ring|alert|silent, payload }`.
     Class is chosen by the *caller* (calls→ring, messages→alert,
     receipts/likes→silent) — `route` never decides urgency.
   - Muted-channel suppression (§10.8): callers pass the recipient's
     membership row when they have it (channels does); `route` downgrades
     to `silent` when `muted`. Core stores nothing else about presentation.
   - Commands `notify.seen { ids }`, `notify.clear`; HTTP
     `GET /v1/notify/inbox?cursor` lands with the shared cursor util in
     Sprint 4 — this sprint it takes `?limit` only (newest N, no paging)
     to avoid inventing a throwaway idiom. Mark the gap with a tracked
     TODO that Sprint 4's checklist closes.
2. **Channels schema** (§10.2): all five tables. `messages` is
   `PARTITION BY RANGE (created_at)` from the first migration — retrofit
   partitioning is a rewrite; do it now. Migration creates the current and
   next month partitions; automated creation lands in Sprint 11, and the
   janitor gets a stopgap task now: on tick, `CREATE TABLE IF NOT EXISTS`
   next month's partition (idempotent, no drop logic yet).
   Unique `(channel_id, client_uuid)` and `(channel_id, seq)` as
   composite-with-partition-key indexes (Postgres requires partition key
   in unique indexes on partitioned tables — `created_at` must join the
   unique constraint; **algorithm consequence**: idempotency dedup cannot
   rely on the DB constraint alone across months. Handle it in the
   handler: `SELECT id, seq FROM messages WHERE channel_id=$1 AND
   client_uuid=$2` *before* insert (index-backed), then insert; the
   unique index still guards the common same-partition race, and the
   pre-check closes the cross-partition edge. Document this in
   `store.rs` — it is the kind of subtlety the next developer deletes as
   "redundant".)
3. **Send hot path** (§8) exactly as the design pseudocode:
   `UPDATE channels SET last_seq = last_seq + 1 … RETURNING` inside the
   insert tx (per-channel serialization via the row lock), persist-then-ack,
   post-commit fan-out: local `subs::publish`, conditional Redis `PUBLISH`
   (payload = the serialized `Evt`, serialized once), then
   `notify::route` per offline member (members with no live session —
   one registry pass over the member list; membership list comes from the
   same tx's `SELECT … FROM channel_members`).
   - Attachment authz at send (§10.2): every id in `body.media_ids` must be
     a `live` media row owned by sender — one `SELECT count(*)` in-tx,
     mismatch → `forbidden`. (Media doesn't exist until Sprint 5; gate the
     check behind the rows existing — i.e. write it now against the schema,
     it simply always fails until media lands, and the test seeds rows
     directly.)
   - `gif_url` host allowlist (config list) at send time → `invalid` if
     not matched.
   - Body validation: total serialized body ≤ 8 KB → `too_large`;
     must contain at least one of `text | media_ids | gif_url`.
4. **`channels.open_direct { number }`** (§10.2, §17.1): resolve number via
   `directory::resolve` seam — directory doesn't exist yet, so implement
   `pub fn resolve(number) -> Option<CharacterId>` *in its final home*
   (`primitives/directory/mod.rs`) now, reading `characters.number`;
   blocks-checking joins it in Sprint 5. Found-or-create with unique index
   on the **ordered pair**: computed columns/index
   `UNIQUE (world_id, kind, least_char, greatest_char)` where
   `least/greatest` = the two member ids sorted — store them as explicit
   columns on `channels` (`pair_a`, `pair_b`, null for groups); INSERT …
   ON CONFLICT returns the existing channel. No member-set hashing.
5. **`channels.create`** (groups): creator + explicit member list (cap 32,
   `invalid` beyond), kind=group; inserts channel + members in one tx.
   `channels.list`: own memberships snapshot (channel row + own membership
   watermarks + last message preview via lateral join, one query).
6. **`sub` authorization for `ch:*`**: membership lookup; register topic.
   Resume replay is Sprint 4; this sprint `last_seq` in sub is accepted
   and ignored-with-log (protocol shape stable, behavior lands next).
7. Events: `channels.message` (durable). Wire `Evt::class()` for it.

### Test plan

(protocol harness carries most of this)

- Send happy path: ack `{message_id, seq}`; subscriber receives event with
  same seq; ordering across 100 rapid sends is gapless and monotonic.
- Idempotent retry: same `client_uuid` twice → identical ack, one row, no
  second event.
- Concurrent senders: 16 tasks × 50 messages into one channel → seqs are a
  permutation-free contiguous range 1..=800 (this is *the* invariant test;
  it becomes a proptest in Sprint 9).
- open_direct: two concurrent opens of the same pair → one channel;
  reversed argument order → same channel.
- Offline member: no live session → inbox row with class `alert`; muted
  membership → class `silent`.
- Non-member send/sub → `forbidden`.
- RLS: second world with identical ids sees nothing.
- Cross-partition idempotency: seed a message row in last month's
  partition, retry same `client_uuid` now → deduped.

### Exit criteria

- [ ] End-to-end demo script in-repo: two WS clients, one sends, the other
      renders — used in every future manual sanity check.
- [ ] Concurrent-seq test green 100 consecutive runs.
- [ ] p99 of `channels.send` handler < 5 ms at 30 msg/s on the dev machine
      (first perf number of the project; record it in the sprint notes —
      Sprint 10 tracks the trend).

---

## Sprint 4 — Channels complete, pagination idiom, loadgen v0

**Goal**: the messaging surface is feature-complete (receipts, typing,
reactions, pins, members, resume, history), the one pagination idiom exists
for every future read, and the load generator exists so perf smoke starts
running nightly.

**Depends on**: Sprint 3.

### Work items

1. **Cursor util** (`infra/cursor.rs`, CDR-7): encode = base64url-no-pad of
   `serde_json` of `(created_at_micros: i64, id: Uuid)`; decode failure →
   `ErrCode::invalid`. One generic
   `fn page<T>(rows, limit) -> Page<T> { items, next_cursor }` helper that
   takes the +1-row overfetch and emits the cursor from the last returned
   row. Every paginated read from now on uses this — feed, history, inbox,
   gallery, ledger. Retrofit the Sprint 3 inbox read now (closing its TODO).
2. **History** `GET /v1/channels/:id/messages?before_seq&limit` (§6):
   JWT auth (axum extractor verifying via `infra::auth`), membership check,
   keyset on `(channel_id, seq)` descending. This route is seq-keyed (not
   cursor-keyed) per the design table — seq is already public in this
   contract; the cursor idiom is for time-ordered surfaces.
   Cap `limit` at 100.
3. **Resume** (§4.4): `sub { topic: ch:*, last_seq }` → after registration,
   replay `WHERE channel_id=$1 AND seq > $2 ORDER BY seq LIMIT 500` as
   normal `channels.message` events *before* the sub ack (same
   snapshot-before-ack rule as presence — client logic stays uniform).
   Exactly 500 rows returned → append event `channels.resume_overflow
   { channel_id }` telling the client to cold-load via HTTP. Order
   guarantee: replay runs on the connection's dispatch task, and live
   events for that topic can interleave *after* registration — dedup is
   the client's job by seq (documented contract, OPN.md §5); server-side
   we guarantee no event with seq ≤ replayed max is lost.
4. **Receipts** (§10.2): `channels.mark_delivered` / `mark_read
   { channel_id, up_to_seq }` — watermark update guarded
   `SET last_read_seq = $s WHERE … AND last_read_seq < $s` (monotonic,
   idempotent, no-op acks ok). Emits `channels.receipt` (durable) with
   server `now()` as `at`. No per-message rows, ever.
5. **Typing**: `channels.typing { channel_id }` → ephemeral event to topic,
   no persistence, rate class `social`. Client is expected to send at most
   1/3 s; server does not debounce (bucket handles abuse).
6. **Reactions**: insert/delete keyed `(message_id, character_id, emoji)`;
   emoji validated against a small grapheme allow-pattern (single grapheme
   cluster, ≤ 8 bytes) — not an emoji database. Message-exists +
   membership in one query. Event durable.
7. **Pins** (§10.2): pin/unpin; cap 50 enforced in-tx
   (`SELECT count(*) FROM channel_pins WHERE channel_id=$1 FOR UPDATE` —
   the count-then-insert race is closed by locking the channel row instead:
   `SELECT 1 FROM channels WHERE id=$1 FOR UPDATE` first; cheaper than a
   table lock and already the serialization point of the channel).
8. **Members**: `member_add/remove` — group kind only (`conflict` on
   sms/dm), adder must be member (v1 policy: any member may add/remove;
   roles are not in schema — deferred with guilds, §17.2). Events durable;
   removed member's topic registration dropped server-side (registry
   lookup by session).
9. **`opn-loadgen` v0** (`crates/loadgen`): tokio binary reusing
   `contracts`. Scenario config (TOML): N connections, world/tenant seed
   via a `--seed` mode that calls the mint API, per-conn behavior script
   (send rate, channel population, read/typing mix). Measures: ack RTT
   histogram (hdrhistogram), event delivery latency (send timestamp →
   receipt at subscriber, same process so clock-safe), drops, closes.
   Output: one JSON summary line for CI assertion + human table.
   Wire the nightly CI perf smoke (cross-cutting rule 5): 300 conns,
   30 msg/s, 5 min, assert p99 ack < 25 ms, zero durable closes.
10. **Last seen** (§10.2): already written on disconnect (Sprint 2);
    expose in `channels.list` / `identity.me` payloads honoring
    `share_presence` at read time.

### Test plan

- Resume: kill socket mid-stream, reconnect with `last_seq`, assert exact
  gap replay then live continuation, no dupes below replayed max;
  >500 gap → overflow event.
- Receipts: monotonicity (regress attempt no-ops), both kinds, event `at`
  populated; delivered-fires-with-app-closed semantics = just a normal
  client sub (no special server path — assert nothing special exists).
- Pins cap: 50 then `conflict`; concurrent pin race at 49 → exactly 50.
- History: pagination walks the full set without dup/skip while a writer
  inserts concurrently (keyset property).
- Reactions/members/typing: happy + authz-negative each.
- Loadgen smoke asserted in CI (first nightly run green).

### Exit criteria

- [ ] Every `channels.*` command in the coverage match-test.
- [ ] Nightly perf smoke live and green three consecutive nights.
- [ ] Messages surface demo-able end to end against the real shell dev
      build if available (coordination point with opn-ui; not a blocker).

---

## Sprint 5 — Media + directory

**Goal**: bytes flow client↔MinIO without touching Core, with caps that
cheating clients cannot bypass persistently; contacts/blocks/listings exist
and blocks actually gate contact surfaces.

**Depends on**: Sprint 4 (cursor util; channels attachment check goes live).

### Work items

1. **Media schema** (§10.6) + RLS.
2. **`media.request_upload { kind, bytes, mime }`**: validate kind/mime
   pair and size caps (photo ≤ 2 MB, video ≤ 25 MB, audio ≤ 1 MB, thumb
   ≤ 40 KB; MIME allowlist per kind, e.g. photo: `image/jpeg|png|webp`).
   Insert `pending` row, return **presigned POST policies** (not bare
   PUT): S3 POST policy supports `content-length-range` and exact
   `Content-Type` conditions — this is the mechanism that makes caps
   MinIO-enforced rather than advisory (OPN.md §7.2). Two policies:
   original + thumb (photo/video only; audio none). Expiry 10 min.
   Object keys: `w/<world_id>/<media_id>` and `…_t` — immutable, so
   `Cache-Control: public, max-age=31536000, immutable` set via
   post-policy metadata; "content-addressed" in the design docs is
   satisfied by immutability-per-key (the id never re-points), no client
   hashing. Use `rust-s3` or `aws-sdk-s3` against MinIO — pick
   `aws-sdk-s3` (maintained, presigned POST support) unless its size
   offends; do not hand-roll SigV4.
3. **`media.commit { media_id }`**: owner check, `pending → live`, no
   synchronous HEAD (§17.3 decided). `media.favourite` toggles the
   lifecycle-exempt tag (set object tag via S3 API in the same handler —
   the one place Core talks to MinIO metadata).
4. **Janitor sweeps** (§10.6):
   - pending rows > 15 min → delete row + best-effort `DeleteObject`.
   - **live verification**: batch of live rows not verified in the last
     24 h (add `verified_at` column), HEAD each
     (`futures::stream::iter(...).buffer_unordered(16)`): missing object
     or `content_length > declared bytes` → revert to `pending` (next
     sweep deletes), log + metric. Cursor over `(verified_at NULLS FIRST,
     id)` so the sweep is incremental, ≤ 500 rows per tick — never a full
     table scan per tick.
   - lifecycle-expired objects: HEAD 404 on old rows hits the same
     missing-object path; row marked `expired` (new state) so galleries
     render tombstones instead of broken fetches.
5. **`media.list`** (HTTP, own gallery, cursor idiom).
6. **Channels attachment check goes live** (Sprint 3 item 3): un-gate, add
   the protocol test (send with foreign/pending media id → `forbidden`).
7. **Directory** (§10.7): `contacts` CRUD (PK `(owner_character, number)`,
   contacts point at numbers; display fields free-form; `avatar_media`
   validated owned-if-present), `blocks` (block by number),
   `directory.resolve { number }` → **opaque routing**: returns an
   ephemeral `resolve_token` (HMAC over `(world, number, day)` — or
   simpler and chosen here: resolve returns only *whether* the number is
   reachable plus display metadata; actions (`open_direct`, `calls.start`)
   take the raw `number` and re-resolve internally. No character id ever
   crosses the wire from resolve — this satisfies "never leaks the
   character behind a number" with zero token machinery).
   `listings` CRUD with `expires_at` (janitor deletes expired), cursor
   reads.
8. **Block enforcement at action points** (§10.7): `channels.open_direct`
   and (Sprint 6) `calls.start` check both directions
   (`blocker=callee,blocked=caller_number` and the inverse) →
   behave as unreachable: `not_found`, indistinguishable from
   no-such-number (privacy: a block must not be detectable). Document this
   equivalence in the handler.

### Test plan

- MinIO integration (compose): request→POST→commit happy path; oversize
  POST rejected *by MinIO* (assert the 4xx comes from the policy, not
  Core); wrong MIME rejected; commit of foreign media → `forbidden`.
- Janitor: orphan pending reaped; live row with deleted object reverted;
  oversized object (uploaded via a second, laxer policy in test setup)
  caught by verification.
- Directory: contact CRUD; resolve unknown vs blocked number both
  `not_found`-equivalent (same wire bytes); listings expiry.
- open_direct blocked-pair test (both directions).

### Exit criteria

- [ ] A real photo round-trips dev-stack: request → upload → commit →
      appears in `media.list` → attaches to a message → other client
      fetches via presigned GET. Scripted, in-repo.
- [ ] Verification sweep provably catches a cap bypass (test above green).
- [ ] All media/directory commands in coverage match-test; HTTP routes in
      route-coverage test.

---

## Sprint 6 — Calls + tenant link

**Goal**: voice/video call sessions with a crash-proof state machine,
opaque WebRTC signaling relay, and the one-directional tenant link
delivering voice-target events with re-sync.

**Depends on**: Sprint 5 (blocks at `calls.start`; notify ring class from
Sprint 3).

### Work items

1. **Schema** (§10.4) + RLS.
2. **State machine — implement as data, not scattered ifs**: one transition
   table in `primitives/calls/fsm.rs`:
   ```
   session: Ringing --accept--> Active --last_hangup--> Ended
            Ringing --decline_all/timeout/caller_hangup--> Ended
   participant: Ringing --accept--> Joined | --decline--> Declined
                Joined --hangup--> Left
   ```
   `fn apply(session_state, participant_states, event) -> Result<Transition, ErrCode::Conflict>`
   as a pure function (proptest target in Sprint 9). Handlers load rows
   `FOR UPDATE` (lock order: session row first, then participants),
   apply, persist, emit. Illegal transition → `conflict`. Terminal rule:
   nothing leaves `Ended`, enforced by the pure function having no such
   arm.
3. **Commands**:
   - `calls.start { callee_number, video }`: resolve via directory seam
     (block check, §5 item 8 semantics); callee busy (any participant row
     in a non-ended session) → ack `conflict` with `busy` detail; create
     session (`ringing`), caller participant `joined`, callee `ringing`;
     ring via `notify::route(class=ring)` carrying `call_id` — dialer
     needs no standing sub (§10.4).
   - `calls.accept`: FSM; session → `active`; emit voice-target event on
     tenant link (`set_targets` with both characters); snapshot event to
     `call:<id>`.
   - `calls.decline` / `calls.hangup`: FSM; when the transition ends the
     session → link event `clear`, snapshot `ended`.
   - `calls.signal { call_id, to, payload }`: sender and `to` must both be
     non-declined participants of a `ringing|active` session; payload
     opaque (size cap 16 KB), relayed as `calls.signal` event to `to`'s
     sessions. Never persisted, never inspected. Class ephemeral?
     **No — durable**: a dropped ICE candidate stalls call setup; the
     queue-full close is the correct failure. Note this in `Evt::class`.
   - `sub call:<id>`: participants only, snapshot-on-sub of full session
     state (CDR-6).
4. **Events**: `calls.state` = full snapshot every change (small; kills
   delta desync). Ring delivery via notify (already above).
5. **Janitor**: sessions `state != ended` with zero `joined` participants
   and `created_at < now() - 60 s` → force-end + link `clear` (no zombie
   rings, §10.4).
6. **Tenant link** (§5):
   - `GET /link` WS upgrade authed by API key header (reuse `TenantAuth`).
     One connection per tenant, last-writer-wins (same takeover mechanism
     as sessions — reuse the pattern, small registry
     `DashMap<TenantId, LinkHandle>`).
   - Hello frame from resource: `{ resource_version, contracts_version }`;
     log pair; known-broken combo list (config, empty at v1) → close 4409.
   - **Down only**: `calls.voice { call_id, action: set_targets|clear,
     characters }` — same envelope/`Evt` types from contracts. Tenant
     disconnected → events dropped by design.
   - `GET /v1/tenants/self/calls/active` (API key): active sessions +
     joined participants — the resource re-syncs on link connect. Cursor
     not needed (bounded by concurrent calls).
   - Backpressure: same bounded queue; link events are durable-class
     (queue full → close; resource reconnects and re-syncs — the re-sync
     route is what makes drop-on-close safe).
7. **coturn**: not Core code, but Sprint 6 owns adding coturn to the
   compose stack + documenting the STUN/TURN config the NUI will receive
   (static config via tenant config → included in `calls.state` snapshot
   payload as `ice_servers`). Keeps Sprint 6 the "everything video calls
   need from the backend" sprint.

### Test plan

- FSM unit tests: every legal transition; every illegal one → `conflict`;
  terminal absorption (`Ended` + any event → `conflict`).
- Protocol: full call lifecycle (start → ring notify at callee → accept →
  both get snapshots → signal both directions → hangup → ended + link
  `clear`); decline path; busy path; block path (`not_found`, same as
  unknown number); non-participant signal → `forbidden`.
- Janitor: crash-abandoned ring (insert rows directly) reaped at 60 s,
  link `clear` emitted.
- Link: hello handshake; takeover; disconnect → events dropped without
  error; reconnect + re-sync returns the active call; queue-full close.

### Exit criteria

- [ ] Scripted two-client + fake-link demo: call connects, link receives
      `set_targets`, hangup clears.
- [ ] FSM is a pure function with 100 % transition-table test coverage
      (every cell of states × events asserted).
- [ ] All `calls.*` in coverage test; `/link` + re-sync in route test.

---

## Sprint 7 — Ledger + exchange

**Goal**: money that cannot be created, destroyed, or double-spent — with
the framework exchange protocol and nightly reconciliation that turns any
silent corruption into a detected freeze within 24 h.

**Depends on**: Sprint 3 (gateway + notify). Independent of 5/6 —
parallelizable with a second developer.

### Work items

1. **Schema** (§10.5) + RLS, with two additions closed here:
   - `accounts.frozen_at timestamptz` — freeze mechanism for
     reconciliation (outgoing ops on frozen account → `conflict`).
   - `CHECK (balance >= 0 OR owner_kind = 'system')` — the tenant `system`
     account is the mint/sink for exchange and may run negative (it
     represents money that exists in the framework, not in the ledger).
   - `exchanges (id text PK per (world), world_id, character_id, amount,
     direction, state, created_at)` — idempotency + audit for the exchange
     protocol (PK `(world_id, id)` since `exchange_id` is bridge-chosen).
2. **Transfer algorithm** (one tx, deadlock-free):
   ```
   world_tx:
     -- idempotency first
     SELECT id FROM transfers WHERE from_account=$f AND client_uuid=$c
       → hit: return stored ack, COMMIT (no-op)
     -- lock both rows in deterministic order
     SELECT id, balance, frozen_at FROM accounts
       WHERE id IN ($f,$t) ORDER BY id FOR UPDATE      -- always id-order: no deadlock
       → missing row → not_found; frozen source → conflict
     -- available = balance − active holds
     SELECT COALESCE(SUM(amount),0) FROM holds
       WHERE account_id=$f AND state='held'
       → balance − held < amount → conflict (insufficient)
     UPDATE accounts SET balance = balance − $a WHERE id = $f;
     UPDATE accounts SET balance = balance + $a WHERE id = $t;
     INSERT INTO transfers (...);
   COMMIT
   ```
   The `FOR UPDATE … ORDER BY id` line is the load-bearing choice —
   two opposing concurrent transfers cannot deadlock. The CHECK constraint
   is the backstop, not the mechanism (a CHECK violation acks `internal`,
   not `conflict` — if it ever fires, the available-balance logic has a
   bug; log accordingly).
3. **Holds**: `ledger.hold` (same lock + available check, insert `held`
   with `expires_at`), `ledger.capture { hold_id, to }` (lock hold +
   accounts id-order; `held → captured`; insert transfer `kind=capture`
   moving from the holding account to `to` — balance was never moved at
   hold time, so capture debits now; available-math already excluded it),
   `ledger.release` (`held → released`). Hold states are a 3-state FSM —
   same pure-function pattern as calls, proptest target.
4. **Exchange protocol** (§10.5, OPN.md §14.2):
   - `POST /v1/tenants/self/exchange { exchange_id, character_id, amount,
     direction: deposit|withdraw_confirm }` (API key):
     - `deposit`: upsert `exchanges` by PK — existing → return stored
       result (idempotent); else in the same tx: transfer
       `system → character wallet` (auto-create wallet account on first
       touch, `owner_kind=character`, currency from tenant config).
     - `withdraw` is two-legged: WS `ledger.withdraw { amount }` creates a
       hold on the wallet **plus** an `exchanges` row
       (`state=pending_confirm`, id minted by Core, returned to client →
       relayed to bridge via game plane); bridge credits framework bank,
       then calls `withdraw_confirm` → capture hold to `system`, exchange
       `state=done`. Unconfirmed → hold expiry auto-release (janitor) +
       exchange `state=expired`. Debit-source-first rule holds on both
       directions.
   - `GET /v1/tenants/self/exchange?since` (API key): journal read for the
     bridge's reconciliation, keyset on `(created_at, id)`.
5. **`ledger.history`** (HTTP, cursor idiom): own accounts' transfers.
6. **Janitor**: expired `held` holds → `released` + notify owner
   (`silent`).
7. **Nightly reconciliation** (janitor task, runs at a config hour, still
   under the 30 s tick with an hour gate + advisory lock):
   ```
   per world (streamed, one account page at a time):
     recomputed = SUM(incoming transfers) − SUM(outgoing transfers) per account
     recomputed != balance → UPDATE accounts SET frozen_at = now();
                             log error + metric opn_ledger_drift_total
   plus: SUM over exchanges vs system-account transfer legs (cross-check)
   ```
   One SQL statement per world does the whole per-account comparison
   (`GROUP BY account` join `accounts`) — no row-at-a-time loop in Rust.
   Unfreezing is a manual CLI subcommand (`opn-core admin unfreeze`) —
   deliberate human gate, per design.
8. **Notify integration**: incoming transfer → `notify::route`
   (class `alert`, app_id `wallet`).

### Test plan

- `#[sqlx::test]`: transfer happy/insufficient/frozen/missing; idempotent
  retry returns identical result; hold available-math (hold then
  overspend attempt → `conflict`); capture/release/expiry FSM; negative
  system balance allowed, negative character balance impossible (CHECK
  proven by direct SQL attempt).
- **Concurrency battery** (pre-proptest): 16 tasks random transfers among
  8 accounts, then assert global invariant: `SUM(all balances) == 0`
  relative to start and per-account recompute matches — the same query
  reconciliation uses (test and prod share the invariant SQL — one source
  of truth, `store.rs` fn).
- Opposing-pair transfer storm (A→B and B→A, 200 iterations) → zero
  deadlock errors.
- Exchange: deposit idempotency (same `exchange_id` 5×, one credit);
  withdraw full cycle; withdraw expiry releases; journal read matches.
- Reconciliation: hand-corrupt a balance via SQL → nightly task freezes
  the account, outgoing op → `conflict`, unfreeze CLI restores.

### Exit criteria

- [ ] Concurrency battery green 100 consecutive runs.
- [ ] Reconciliation catches an injected corruption in test.
- [ ] Exchange protocol documented for the bridge author (short section
      appended to this file or `docs/opn-bridge-exchange.md`) with the
      exact idempotency and replay rules — the bridge is other-repo code;
      the contract must not live in Slack messages.

---

## Sprint 8 — Feed

**Goal**: the social primitive, built to the fan-out-on-read design, fully
tested — even though no v1 app consumes it (OPN.md §14.5: primitive ships,
app deferred).

**Depends on**: Sprint 4 (cursor, media attachment pattern). Parallelizable
with 6/7.

### Work items

1. **Schema** (§10.3) + RLS. Indexes are the sprint's core deliverable:
   - timeline: `(world_id, app_id, author_account, created_at DESC, id DESC)`
     on `posts`;
   - follows lookup: PK `(world_id, app_id, follower_account, followee_account)`;
   - `likes` PK `(post_id, account_id)`; `hashtags` `(world_id, app_id, tag, post_id)`.
2. **Commands**: `feed.post` (body ≤ 4 KB, media ownership check reused
   from channels — extract the shared check into `media::assert_owned_live`
   now that two callers exist), `feed.delete` (author only; hard delete —
   cascades likes/comments/hashtags in one tx), `feed.like/unlike`
   (counter `UPDATE … SET like_count = like_count + 1` same-tx, §10.3),
   `feed.comment` (+ `comment_count`), `feed.follow/unfollow`.
   Hashtags parsed server-side at post time (`#[\p{Alnum}_]{1,32}`,
   ≤ 10 per post, lowercased) — parse once at write, never at read.
3. **Reads** (HTTP, cursor idiom): home timeline —
   ```sql
   SELECT p.* FROM posts p
   WHERE p.world_id=$w AND p.app_id=$a
     AND (p.author_account = $me OR EXISTS (
       SELECT 1 FROM follows f WHERE f.world_id=$w AND f.app_id=$a
         AND f.follower_account=$me AND f.followee_account=p.author_account))
     AND (p.created_at, p.id) < ($c_ts, $c_id)
   ORDER BY p.created_at DESC, p.id DESC LIMIT $n
   ```
   (EXISTS form rather than JOIN — no dedup needed when following +
   self-posting; verify plan uses the timeline index with a seeded 100 k
   post `EXPLAIN` test). Also: profile timeline (by author), post detail +
   comments (cursor), hashtag page.
4. **Events** (advisory only, §10.3): `feed.activity` on
   `feed:<world>:<app>` — ephemeral class (advisory by definition; a lost
   one costs a refresh). Own-post-liked notification → `notify::route`
   (`silent`).
5. **Sub authorization** for `feed:*`: any authenticated character of the
   world with an app account for that app.

### Test plan

- Timeline correctness: follows + own posts, no dupes/skips under
  concurrent posting while paginating (keyset property test, same pattern
  as history).
- Counter exactness: 32 concurrent likes → `like_count = 32`; unlike idempotent
  (like twice = one row via PK conflict-ignore, count guarded by insert
  success).
- Delete cascades; comment on deleted post → `not_found`.
- `EXPLAIN` test: timeline query at 100 k seeded posts uses the index
  (assert no seq scan in plan; brittle but cheap — gate on
  `Seq Scan on posts` absence).
- Advisory event received by subscriber; non-account sub → `forbidden`.

### Exit criteria

- [ ] All `feed.*` commands + routes in coverage tests.
- [ ] 100 k-row EXPLAIN test green.
- [ ] p95 timeline read < 10 ms at 100 k posts on dev hardware (recorded).

---

## Sprint 9 — Verification hardening

**Goal**: the stability-grade verification layer from OPN-CORE.md §15 —
property tests on every invariant, fuzzing on the attacker-controlled
surface, chaos drills as repeatable scripts. This sprint exists because
ADR-1's amendment makes it budgeted work, not polish.

**Depends on**: Sprints 3–8 (the code under test).

### Work items

1. **Property tests** (`proptest`), each with an explicit model:
   - **Ledger**: strategy generates op sequences
     (`Transfer|Hold|Capture|Release|ExchangeDeposit|Withdraw` with
     amounts/accounts from small pools, including invalid ones); execute
     against real Postgres; after each sequence assert: per-account
     recompute == balance (the reconciliation SQL), no negative
     non-system balance, ∑ holds active ≤ balance per account, captured
     holds have matching transfers. Concurrent variant: split the
     sequence across 8 tasks, assert only the global invariants.
   - **Channels**: op sequences of sends (some with duplicate
     `client_uuid`s) from concurrent tasks → seq per channel is gapless
     `1..=n`, dup uuids produced identical `(message_id, seq)` acks,
     event count == distinct sends.
   - **Calls FSM**: arbitrary event streams against the pure
     `fsm::apply` — never panics, never leaves `Ended`, participant/
     session state combinations stay within the legal set (encode the
     legal-set predicate once).
   - **Cursor**: encode/decode round-trip; arbitrary bytes decode to
     `invalid`, never panic.
   Property tests run in CI nightly (256 cases), locally shrunk
   (`PROPTEST_CASES=16`) in the normal suite.
2. **Fuzzing** (`cargo-fuzz`, §15): targets:
   - `fuzz_client_frame`: `serde_json::from_slice::<ClientFrame>` +, on
     success, the *validation* layer (topic parse, body caps) — the full
     pre-handler path. Crash = bug by definition.
   - `fuzz_link_hello`, `fuzz_cursor_decode`.
   CI: 5 min per target per night; corpus committed. Local: `just fuzz`.
3. **Chaos drills** (§15) — each a script in `chaos/` runnable against the
   compose stack, each asserting its invariant machine-readably (exit
   code), all runnable by one `just chaos`:
   - `kill9-mid-send.sh`: loadgen at 30 msg/s, `kill -9` core, restart,
     assert every acked message is present and replayed to a resuming
     client (loadgen records acked ids; verifier compares).
   - `pg-restart.sh`: restart Postgres under load; assert core recovers
     (pool reconnects), zero acked-but-lost, error acks (not silence)
     during the gap.
   - `redis-restart.sh`: restart Redis; assert presence keys rebuild
     within one heartbeat cycle and pub/sub resubscribes (two-instance
     mode).
   - `link-drop.sh`: kill the fake link consumer mid-call; reconnect;
     assert re-sync returns the active call and a subsequent accept emits
     targets.
   Weekly CI job runs all four (§15: "run in CI weekly, not a one-time
   manual exercise").
4. **RLS audit**: one generated test per domain table (macro or build
   script over a table list) asserting the two-world isolation property —
   replaces the hand-written canaries with exhaustive coverage.
5. **Dependency audit gate**: `cargo deny` (licenses + advisories) added
   to CI — cheap now, painful later.

### Test plan

This sprint *is* a test plan. Additionally: every bug found by
proptest/fuzz/chaos gets a minimized regression test in the normal suite
before the fix merges — the generative layer finds, the deterministic
layer remembers.

### Exit criteria

- [ ] All four proptest suites green at 1024 cases locally.
- [ ] 24 h fuzz run per target with zero crashes (one-time burn-in;
      nightly stays at 5 min).
- [ ] `just chaos` green three consecutive runs; weekly CI job scheduled.
- [ ] Generated RLS test covers every table with a `world_id` column
      (asserted by the generator diffing `information_schema`).

---

## Sprint 10 — Performance & soak

**Goal**: measured proof of the §7 targets on production-shaped hardware,
bottlenecks found and fixed, and the 24 h/10× soak green — the release
gate for the whole performance premise.

**Depends on**: Sprint 9 (chaos/loadgen infra; verify before you optimize).

### Work items

1. **Loadgen scenarios** (extend `opn-loadgen`):
   - `design`: 300 conns, 30 msg/s aggregate, realistic mix (80 % send,
     10 % typing, 5 % receipts, 5 % reads), channel graph seeded like a
     server (pairs + a few groups).
   - `soak10x`: 3 000 conns, 300 msg/s, 24 h (§15).
   - `reconnect-storm`: drop all 3 000, reconnect with 0–3 s jitter
     (OPN.md §7), measure token-mint burst + replay p99 and time-to-quiet.
   - `hot-channel`: one 100-member group at 10 msg/s (fan-out 1 000
     evt/s) — the fan-out stress shape.
   - `call-churn`: 50 concurrent calls starting/ending at 1 Hz with
     signaling — exercises link + FSM under load.
2. **Measurement discipline**: every scenario emits the JSON summary;
   results land in `perf/results/<date>-<scenario>.json`, committed —
   the trend is the artifact, single runs are noise. Loadgen runs from a
   second machine (or at minimum a separate pinned core set) — a
   colocated generator steals the CPU it is measuring.
3. **Environment**: production-shaped compose on the i5-14500 host:
   Core pinned to E-cores (`cpuset`), `TOKIO_WORKER_THREADS=4`,
   Postgres with `shared_buffers=1–2 GB` (OPN.md §7.1, §11) — perf
   numbers on unpinned dev laptops don't gate anything.
4. **Profiling passes** (in order, stop when targets met):
   `pg_stat_statements` first (DB access patterns dominate, §7), then
   `cargo flamegraph` on core under `design` load, `tokio-console` for
   task stalls, heaptrack if RSS trends up. Known-likely suspects, checked
   not guessed: missing index (every query EXPLAINed at realistic row
   counts), send-path allocations (serialize-once check: one
   `serde_json::to_vec` per event, `Bytes`-shared across subscribers),
   registry lock contention (DashMap shard stats; §4.2 allows swapping to
   sharded Mutex **only if measured**).
5. **Fix + re-measure loop**: each fix is its own commit with
   before/after numbers in the message. No speculative optimization
   without a flamegraph pointing at it (the design's own rule: measure,
   don't guess).
6. **Targets (gate)** — from OPN.md §7 / OPN-CORE.md §14:
   - command processing p99 < 5 ms at `design`; < 25 ms alert line never
     crossed during any scenario except reconnect-storm's first 10 s.
   - zero durable-queue closes at `design` and `soak10x`.
   - `soak10x` 24 h: RSS slope ≈ 0 after warm-up (linear fit over hours
     2–24 < 1 MB/h), fd count flat, p99 hour-24 within 20 % of hour-1,
     zero janitor failures.
   - reconnect-storm: all 3 000 resumed within 60 s, no replay gaps
     (loadgen verifies seq continuity per client).
   - Core RSS at `design` ≤ 200 MB (generous vs the 30–60 MB §7.1
     estimate; tighten to measured+50 % once known).
7. **Nightly perf smoke thresholds tightened** to measured-p99 + 50 %
   margin (replacing the loose Sprint 4 numbers).
8. **Backpressure/limits tuning**: with real numbers, revisit queue
   capacity (256), rate budgets (§12 table), pre-auth caps — adjust
   config defaults, document rationale inline in `config.rs`.

### Test plan

The scenarios are the tests. Deterministic additions: seq-continuity
verifier in loadgen (asserts per-topic gapless delivery under all
scenarios — turning the delivery guarantee into a continuously-checked
property under load).

### Exit criteria

- [ ] All six targets met on the production-shaped environment, numbers
      committed to `perf/results/`.
- [ ] 24 h soak green (one full pass minimum; the release ritual repeats
      it per release, §15).
- [ ] Every fix from the sprint has a regression guard (either the
      tightened nightly smoke or a dedicated scenario assertion).

---

## Sprint 11 — Release engineering

**Goal**: v1.0 deployable by someone who is not the author: automated
partitions, backups, alerts, runbooks, published contracts, and the weekly
verification cadence running without a human.

**Depends on**: Sprint 10.

### Work items

1. **Coolify deployment** (OPN.md §11): production compose — core
   (E-core `cpuset`, `TOKIO_WORKER_THREADS=4`), Postgres 16 (tuned
   `shared_buffers` per §7.1), Redis, MinIO, coturn; Traefik labels (TLS,
   sticky WS — sticky is cosmetic at 1 replica but configured now so
   replica 2 is a scale action, not a config project); `/healthz` gating
   rollout; `OPN_METRICS_BIND` on the internal interface only.
2. **Partition automation** (§9): decide `pg_cron` vs Coolify-cron-hitting
   admin route — **choose `pg_cron`** (no HTTP surface, survives Core
   restarts, one `SELECT cron.schedule(...)` migration): create month N+1
   on the 20th, drop partitions past retention (config, default 90 d).
   The Sprint 3 janitor stopgap for partition creation is removed —
   delete the code, don't leave two owners of one job.
3. **Backups**: Coolify scheduled Postgres dumps to S3-compatible storage;
   MinIO bucket replication or scheduled `mc mirror` for media (decide by
   storage cost; media loss = cosmetic, DB loss = fatal — DB backup is
   the non-negotiable). **Restore drill is part of the sprint**: one
   scripted restore into a fresh stack, verified by row counts + a
   message-read smoke — an untested backup is a wish, not a backup.
4. **Alerts** (§14, exactly the four): p99 > 25 ms sustained 5 min,
   durable-queue closes > 0, PG pool exhaustion, janitor failures — plus
   two ops-level ones: `/healthz` down, disk > 80 %. Prometheus rules +
   whatever notifier the host already runs. No dashboards beyond one
   overview (the §14 anti-goal: no dashboards-for-dashboards).
5. **Runbooks** (`docs/runbooks/`): one page each — deploy/rollback,
   restore-from-backup, frozen-account investigation (reconciliation
   fired), replica-2 scale-up (the §7.1 path), incident triage order
   (healthz → metrics → logs by span). Each runbook's commands are
   copy-pasteable and were executed once during this sprint.
6. **Contracts publish**: `@opn/contracts` npm publish pipeline on git
   tag; semver policy documented (additive-only within major, OPN.md
   §10.1); `contracts_version` embedded in the crate (`env!` from
   `CARGO_PKG_VERSION`) and reported in the link hello + `/healthz` body.
7. **Release ritual documented**: tag → CI (full suite + fuzz smoke) →
   soak (24 h, §15) → chaos suite → deploy → post-deploy smoke script.
   Weekly CI: chaos + 1 h mini-soak (already wired in 9/10 — verify the
   schedule actually fires and pages on red).
8. **Security pass**: `cargo deny` clean; secrets never in logs (grep
   audit for token/key in tracing fields); JWT secret rotation procedure
   (mint with new, verify with either, one-deploy overlap) documented in
   a runbook; Traefik TLS config (min 1.2, HSTS) checked.

### Exit criteria

- [ ] A clean VM + the deploy doc → running production stack, done by
      someone other than the author (or the author following only the
      doc, screen-recorded).
- [ ] Restore drill passed.
- [ ] Alert test-fires verified (kill PG, watch the page arrive).
- [ ] `@opn/contracts@1.0.0` published; tag `opn-core v1.0.0` cut after
      one full release ritual pass.

---

## Testing strategy summary (how the API stays covered)

Mechanisms, all compiler- or CI-enforced rather than discipline-enforced:

1. **Exhaustive `Cmd`/`Evt` match-tests** (rule 2) — a command cannot exist
   without a named integration test.
2. **Route-coverage test** (rule 3) — same for HTTP.
3. **Protocol harness** (Sprint 2) — every primitive's happy path, resume,
   and idempotency tests speak the real wire protocol over a real socket;
   they double as living contract examples (§15).
4. **`#[sqlx::test]` store tests** — real Postgres, per-test DB, RLS-on
   role; no mocked DB anywhere (§15).
5. **Property tests** (Sprint 9) — the invariants (money conservation, seq
   gaplessness, FSM legality) hold under generated adversarial sequences.
6. **Fuzzing** (Sprint 9) — the untrusted input surface cannot panic.
7. **Chaos scripts** (Sprint 9, weekly) — the delivery guarantees hold
   across crashes and restarts.
8. **Loadgen scenarios** (Sprints 4/10, nightly + release) — the perf
   targets and delivery guarantees hold under load, continuously.

Layer 1–4 grow with every sprint; 5–8 are hardening sprints' deliverables
that then run forever in CI. Nothing on this list is a one-time exercise.

## Performance testing strategy summary

- **Continuous**: nightly 5-min smoke at design load from Sprint 4
  (loose thresholds → tightened post-Sprint 10). Catches regressions the
  week they land, not at release.
- **Deep**: Sprint 10's scenario battery on production-shaped, core-pinned
  hardware; profiling only against flamegraph/pg_stat evidence; results
  committed so the trend is reviewable.
- **Release gate**: 24 h 10× soak + chaos suite per release (§15) — the
  "phone that degrades on day 3" failure class is tested for, not hoped
  against.
- **In-prod**: the four §14 alerts watch the same numbers the gates
  enforced.

## Risks worth naming (and where they're parried)

| Risk | Parried by |
|---|---|
| Partitioned `messages` breaks idempotency uniqueness across months | Sprint 3 item 2 (pre-check + index, documented) |
| Ledger deadlocks under opposing transfers | Sprint 7 id-ordered `FOR UPDATE`; storm test |
| RLS forgotten on a new table | Sprint 9 generated per-table test diffing `information_schema` |
| Loadgen colocation poisons perf numbers | Sprint 10 item 2 (separate machine / pinned cores) |
| Contracts drift vs published types | CI drift gate from Sprint 0; publish pipeline Sprint 11 |
| Cap bypass via presign abuse | POST policies (MinIO-enforced) + janitor verification (Sprint 5) |
| Zombie call state after crashes | Pure FSM + janitor reap + link re-sync (Sprint 6) |
| Silent money corruption | Nightly reconciliation + freeze (Sprint 7); proptest (Sprint 9) |
| Slow-consumer memory blowup | Bounded queues + durable-close policy (Sprint 2); soak (Sprint 10) |

---

*Amendments to this roadmap follow the same rule as the design doc: change
the plan here first (with a dated note), then the code. A sprint's scope may
shrink by moving items later, never by deleting their tests.*
