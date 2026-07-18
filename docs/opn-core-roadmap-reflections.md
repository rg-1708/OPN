# OPN-CORE Roadmap — Implementation Reflections

Running log, one section per work session. Newest first. Companion to
[opn-core-roadmap.md](opn-core-roadmap.md); design-level amendments still go
to OPN-CORE.md as CDRs — this file records *how the build actually went*.

---

## 2026-07-18 (evening) — Sprint 4 **part A** (channels feature-complete + cursor idiom): built, green; loadgen (part B) deferred

Deliberate stop mid-sprint. Sprint 4 has two disjoint halves: **(A)** the
messaging surface goes feature-complete and the one pagination idiom lands, and
**(B)** `opn-loadgen` v0 + the nightly perf smoke. Part B is a whole separate
crate whose exit criterion ("nightly smoke green three consecutive nights")
cannot even *finish* in one session, so this session did all of A, stopped, and
reflected. This is pacing within a sprint, not a scope-shrink to a later sprint
— no roadmap amendment needed (item 9 stays in Sprint 4).

### What exists now

- **Migration `0006_reactions_pins.sql`** — the two tables deferred from Sprint 3
  (decision 5 there). Both carry the 0001 NULLIF RLS convention. Neither
  foreign-keys `messages` (partitioned parent → PK `(id, created_at)` makes a
  bare `message_id` FK impossible); handlers validate message existence with an
  RLS-scoped `SELECT` instead. `reactions` PK `(message_id, character_id,
  emoji)`; `channel_pins` PK `(channel_id, message_id)`, the 50-cap enforced
  in-handler under the channel row lock, not by a constraint.
- **`infra/cursor.rs`** — the one pagination idiom (CDR-7): opaque base64url of
  a `(micros, uuid)` keyset pair, `encode`/`decode` (malformed → `invalid`,
  never a panic), and a generic `page<T>(rows, limit, key)` that takes the
  `limit + 1` overfetch and emits the next cursor from the last kept row. Every
  time-ordered read from here on (feed/gallery/ledger) uses it. **Inbox
  retrofitted onto it** — closes the Sprint 3 `?limit`-only TODO.
- **`primitives/channels/` grew the whole feature surface** (`store.rs` SQL +
  `mod.rs` handlers): receipts (`mark_delivered`/`mark_read`, monotonic
  watermark clamped to `last_seq`, event only on real advance), typing
  (ephemeral), reactions (`react`/`unreact`, change-only events, emoji
  allow-check), pins (`pin`/`unpin`, cap-50 under the channel `FOR UPDATE`
  lock), members (`member_add`/`member_remove`, group-only), `resume_replay`,
  `history`, and DM counterpart `last_seen_at` in `channels.list` (share-presence
  gated at read time).
- **Resume replay wired into the `sub ch:` dispatch arm** (§4.4): authorize →
  register → replay `seq > last_seq` (ascending, cap 500) as `channels.message`
  events **before** the sub ack → `channels.resume_overflow` if the 500 cap is
  hit exactly.
- **`http/channels.rs`** — `GET /v1/channels/{id}/messages?before_seq&limit`
  (JWT, membership-gated, seq-keyset descending, limit clamped 100). The one
  seq-keyed read (seq is already public in that contract; the time cursor is for
  time-ordered surfaces).
- **`registry::push_to_awaiting`** — backpressuring durable push for resume (see
  the bug below), and `registry::drop_character_topic` — drops a removed
  member's live `ch:` subscription across their sessions.
- **Contracts** — 9 new `channels.*` `Cmd`s, 6 new `Evt`s (each declaring its
  `class()`), `ReceiptKind` + `MessageItem` types, `ChannelSummary.last_seen_at`.
  Bindings regenerated (`MessageItem`/`ReceiptKind` needed explicit `export_ts`
  entries — `MessageItem` rides HTTP, unreachable from the Cmd/Evt graph).
- **Wiring** — dispatch arms + `wire_name`, `class_of` (receipts→Read,
  everything else social; send stays Msg), `Cmd`/`Evt` coverage match-tests
  extended.
- **Tests** — **115 green across the workspace** (was 96; +2 `#[ignore]`
  soak/bench). New: 4 `cursor` unit tests + 13 integration across four disjoint
  files — `channels_receipts` (3), `channels_reactions_pins` (3),
  `channels_members_resume` (4), `channels_history` (3).

### Decisions closed during implementation

1. **Watermarks clamp to `channels.last_seq`**, not just monotonic-guard. A
   client marking `up_to_seq = 99` on a 3-message channel sets the watermark to
   3, never 99 — `SET last_read_seq = LEAST($s, (SELECT last_seq …))`. Marking
   past what exists would count unsent-future messages as read.
2. **Receipt emits only on a real advance.** A regress/repeat is an idempotent
   `ok` ack with no event; the mark handler returns `Option<seq>` (`Some` =
   advanced → emit, `None` = member no-op, `Err(Forbidden)` = non-member). The
   member-vs-nonmember distinction needs one extra indexed read on the no-op
   path because a zero-row guarded UPDATE can't tell "already read" from "not a
   member".
3. **Every change-only handler (react/pin/member) returns `changed: bool` and
   emits exactly one event per real change.** A duplicate add / absent remove is
   a silent no-op — no event spam, and the tests pin `expect_no_evt` on the
   repeat.
4. **Member removal publishes the `added:false` event *before* dropping the
   member's subscription**, so the removed member receives their own removal
   notice on the way out, then their socket goes quiet. Ordering matters: drop
   first and they'd never learn they were removed over `ch:`.
5. **Emoji validation is a byte-cap + no-control/whitespace check, not a
   grapheme segmenter.** The roadmap says "small grapheme allow-pattern, not an
   emoji database" — true grapheme-cluster validation (ZWJ sequences) needs
   `unicode-segmentation`; deferred behind a `ponytail:` note until a real emoji
   is rejected.
6. **`last_seen_at` in `channels.list` is DM-only and share-presence-gated in
   SQL** (a `CASE WHEN pc.share_presence THEN …` lateral). `identity.me`'s own
   last-seen was skipped as YAGNI — a character's own last-seen is meaningless
   while they're online, and no v1 surface reads it.
7. **`react`/`pin`/`unreact` do not FK `messages`** (partitioned); message
   existence is an RLS-scoped `EXISTS`. Documented in `0006` so the next dev
   doesn't "add the missing FK".
8. **Inbox now returns `{ items, next_cursor }`**, not a bare array — the one
   existing Sprint 3 test (`inbox_http_returns_items`) was updated for the new
   envelope. No other consumer exists yet, so the contract change is free now.

### The bug the resume test caught (the session's keeper)

The **`channels_members_resume` agent found a real product bug** in
`resume_replay`, exactly the kind desk-checking misses:

> `resume_replay` bursts up to `RESUME_MAX` (500) `+ 1` **durable** frames into
> the per-connection send queue in a tight loop with no `.await`/drain. But
> `sendq_capacity` defaults to **256**. At the 257th push the queue is full, the
> durable-into-full guard trips `close(SLOW_CONSUMER)` (4409), and a perfectly
> healthy client is killed mid-catch-up — the exact moment resume exists to
> serve. Deterministic on the current-thread test runtime; a latent race in
> prod's multi-thread runtime under any brief socket stall during a ≥256-row
> replay.

The root cause is a category error in the backpressure policy: the slow-consumer
close assumes a *slow reader*, but a full-cap replay is the **server** bursting
faster than the writer drains, not the client being slow. Fix:
`registry::push_to_awaiting` — resume uses `tx.reserve().await` to *wait* for
queue capacity instead of closing, backpressuring the replay to the client's
drain rate; a genuinely dead socket drops the receiver, `reserve` errors, and
the replay stops. `send`/ack paths keep their fail-fast close-on-full (a real
slow reader still gets closed). The agent's test was `#[ignore]`d with the bug
written up; the fix un-ignored it, and it's now green (500 messages + overflow).

This is the third sprint running where the independent test leg caught a defect
the main thread shipped — and the first where the finding was a *runtime
concurrency* bug (queue capacity vs replay burst), not a spec contradiction or a
stale assertion. "Real-runtime-catches-desk-checks" now extends past Postgres to
the async scheduler.

### Exit criteria status (Sprint 4)

| Criterion | Status |
|---|---|
| Every `channels.*` command in the coverage match-test | **PASS** — all 9 new commands + 6 events named; `tests/coverage.rs` exhaustive match compiles. |
| Nightly perf smoke live and green three consecutive nights | **OPEN** — depends on `opn-loadgen` v0 (part B, next session). |
| Messages surface demo-able end to end vs the shell dev build | **N/A this session** — coordination point with opn-ui, explicitly not a blocker. The four new integration suites are the in-repo end-to-end proof. |

Clippy `-D warnings` clean, `cargo fmt --check` clean, full suite green against
the live stack. CI-on-a-remote / first push still open (no push this session),
so the drift gate stays unarmed; bindings are regenerated and commit-ready.

### Reflection

- **The recipe held at four agents.** Main thread wrote all the coupled core
  (migration, contracts, store SQL, handlers, resume wiring, the cursor util);
  four opus agents each owned exactly one `tests/*.rs` and nobody touched
  `common/`. Zero merge conflicts across four parallel files. Three agents found
  nothing (the code survived their adversarial tests); the fourth found the
  resume bug — the value of the independent leg is entirely in that one catch,
  and it paid for all four.
- **"Report the bug, don't fix it" was the right instruction.** The agent
  `#[ignore]`d its failing test with a precise root-cause writeup and left the
  product alone, so the main thread owned the fix (a backpressure-policy call
  that touches the registry's core invariant — not something to delegate). The
  fix + un-ignore was ~15 lines and one test edit.
- **Seed-via-SQL beat seed-via-WS for volume.** Pins-at-49 and resume-at-500
  need many message rows; the send path is rate-limited (Msg class ~1/s), so the
  agents were told to `INSERT` message rows directly through `world_tx`. Worth
  remembering for every future "needs N rows" test.
- **Splitting the sprint was the right call.** Part A is a coherent, shippable
  milestone (the messaging surface a client actually uses); part B (loadgen) is
  infra with a multi-night exit criterion. Bundling them would have produced a
  worse loadgen under time pressure and a less-reviewed feature set.
- **Not committed** — Sprint 4A left in the tree with Sprints 0–3. Committing +
  first push (which arms CI and the drift gate, open since Sprint 0) remains the
  operator's call; the pile is now four sprints tall.

### Next session

1. **Sprint 4 part B** — `opn-loadgen` v0 (`crates/loadgen`): tokio binary
   reusing `contracts`, TOML scenario config, `--seed` mode hitting the mint
   API, per-conn behavior script, hdrhistogram ack RTT + event-delivery latency,
   JSON summary line. Then wire the nightly CI perf smoke (300 conns, 30 msg/s,
   5 min, p99 ack < 25 ms, zero durable closes) — cross-cutting rule 5. Its exit
   criterion needs three green nights, so it must land before it can close.
2. Then **Sprint 5** (Media + directory), which un-gates the `channels`
   attachment check (Sprint 3 decision 6) into the real owned+live count.
3. Still open, minor: online-member badging (Sprint 3 decision 9), the
   Bearer-scheme case-sensitivity shared with `TenantAuth`, and the
   `identity.me` own-last-seen (decision 6 above) — all deferred, none blocking.

---

## 2026-07-18 (later again) — Sprint 3 (Notify + channels hot path): built, all exit criteria pass

The product's spine. A message is now persisted, sequenced, acked, fanned out
live, and inboxed offline — end to end.

### What exists now

- **Migrations `0004_notify.sql` + `0005_channels.sql`.**
  - `inbox` (RLS) — durable landing for notifications whose recipient had no
    live session.
  - `channels`, `channel_members`, and `messages` — the latter
    `PARTITION BY RANGE (created_at)` from migration one, current + next month
    created at apply time. Ordered-pair unique (`pair_a`, `pair_b`) for
    open_direct. All three carry the 0001 RLS convention (NULLIF form).
  - `ensure_message_partition(timestamptz)` — a `SECURITY DEFINER` function
    (owned by the migrate role) so the janitor, running as `opn_app`, can
    create partitions it otherwise lacks DDL rights for.
  - **`reactions` and `channel_pins` deferred to Sprint 4** (see decisions).
- **`primitives/notify.rs`** — `route` (online → push `notify.event` on each
  `notify:<device>`; offline → one `inbox` row; muted → class downgraded to
  `silent`), `seen`, `clear`, `inbox_list`. The one routing choke point every
  other primitive will call.
- **`primitives/channels/`** — `store.rs` (SQL) + `mod.rs` (validation +
  fan-out): the send hot path (§8), `open_direct` (found-or-create pair),
  `create` (groups, cap 32, cross-world member reject), `list` (lateral
  last-message preview), `authorize_sub`. Body validation (8 KB cap,
  at-least-one-field, gif host allowlist, media gate).
- **`primitives/directory/mod.rs`** — the `resolve` seam (number → character)
  in its final home; blocks join it in Sprint 5.
- **Contracts** — 6 new `Cmd` (`channels.send/open_direct/create/list`,
  `notify.seen/clear`), 2 new `Evt` (`channels.message`, `notify.event`, both
  **Durable**), `MessageBody`/`ChannelSummary`/`MessagePreview`/`InboxItem`/
  `NotifyClass`. Bindings regenerated (`export_ts` now lists the two response
  payloads unreachable from the Cmd/Evt graph).
- **HTTP** — `http/auth.rs` `JwtIdentity` extractor (reused by Sprint 4's
  history/gallery/ledger reads) and `GET /v1/notify/inbox?limit`.
- **Wiring** — dispatch arms, `class_of` (`send`→Msg, list/seen→Read, rest→
  Social), `Cmd`/`Evt` coverage match-tests, `registry::online_notify_targets`,
  janitor `message_partition` stopgap task.
- **Tests** — 96 green across the workspace (+2 ignored benches). New: 6
  channel invariants (`channels_seq.rs`), 8 channel protocol tests
  (`channels.rs`), 7 notify tests (`notify.rs`), 5 body-validation unit tests.

### Decisions closed during implementation

1. **The idempotency check runs AFTER the channel row lock, not before the
   insert** — the sprint's load-bearing correctness call. The roadmap's
   "pre-check then insert; the unique index guards the same-partition race" is
   *insufficient*: the partitioned unique index must carry `created_at`
   (partition key), and two concurrent identical `client_uuid` sends get
   different `now()` timestamps → the unique never fires → duplicate rows with
   different seqs. Fix: `UPDATE channels … RETURNING` (row lock) first, then the
   `(channel_id, client_uuid)` pre-check under that lock, and **roll back the
   seq bump on a dedup hit** so no gap forms. The channel lock serializes all
   sends per channel, so the loser sees the winner's committed row. This is the
   same class of subtlety as Sprint 0's NULLIF and Sprint 1's SAVEPOINT.
   Covered by `concurrent_identical_client_uuid` and `cross_partition_idempotency`.
2. **Partition creation is a `SECURITY DEFINER` function**, because `opn_app`
   (NOSUPERUSER, no DDL) cannot `CREATE TABLE`. `search_path = public, pg_temp`
   — `pg_temp` **last** (PG16 hardening: a temp-schema object could otherwise
   shadow an unqualified name and run with owner rights), `public` first so the
   new partition lands there. My first attempt (`pg_catalog` first) made
   `CREATE TABLE` target the catalog → `permission denied`; the live DB caught
   it in one run (again: desk-check misses, real Postgres catches).
3. **`notify.event` is Durable backpressure class.** A silently dropped
   ring/alert is exactly the degradation ADR-1 forbids; a consumer too slow for
   its own notifications is closed and re-syncs the durable truth on reconnect
   (channel watermarks, inbox, later `/calls/active`). Mirrors
   `channels.message`.
4. **Fan-out is split by cost.** Live `ch:` publish (local registry, one
   serialize) runs inline; the offline-member inbox writes (potentially many)
   are `tokio::spawn`ed post-ack — §8's fire-and-forget. Keeps `channels.send`
   fast (p99 1.8 ms) regardless of member count. A crash before the spawn
   completes loses only the badge; the message row is durable and reaches the
   member via resume (Sprint 4).
5. **`reactions` + `channel_pins` tables deferred to Sprint 4.** The roadmap
   front-loads "all five tables" on the retrofit-is-a-rewrite argument — but
   that applies only to `messages` *partitioning*. reactions/pins are
   unpartitioned, have no Sprint 3 consumer and no Sprint 3 tests, so creating
   them now is pure YAGNI. They land with their handlers next sprint (the
   roadmap's own "shrink by moving items later" allowance).
6. **Media attachment check gated OFF** (`media_ids` non-empty → `forbidden`):
   the `media` table does not exist until Sprint 5, so no id can be valid, and
   you cannot query a table that isn't there. Sprint 5 item 6 un-gates this into
   the real owned+live count check.
7. **`gif_url` allowlist is a hardcoded const**, exact-host + https-only (a tiny
   hand-parser, no `url` dep in core). Config only when a deployment needs
   custom providers.
8. **The end-to-end demo is the `send_delivers_to_subscriber` integration
   test** (two real WS clients over a live socket) — it *is* "in-repo, used in
   every future sanity check" and runs in CI, unlike a websocat script that
   needs a seeded key and a running stack.
9. **online members are not badged by send** — channels routes `notify::route`
   only to *offline* members (roadmap §8 wording); online members rely on their
   `ch:` subscription. `route`'s online-push branch exists for Sprint 6 (calls
   ring an online callee who has no standing sub). The review flagged this as
   worth confirming; it is deliberate. If product wants online badging, have
   send call `route` for online non-senders too and let `route` decide.
10. `open_direct` kind = `'dm'`; self-DM → `invalid`; unknown/blocked number →
    `not_found` (privacy: block indistinguishable from no-such-number, Sprint 5).
11. inbox HTTP is `?limit` only; the shared cursor util (Sprint 4 item 1)
    retrofits it — the roadmap's tracked TODO, closed there.

### Exit criteria status

| Criterion | Status |
|---|---|
| End-to-end demo script in-repo (two clients, one sends, other renders) | **PASS** — `channels::send_delivers_to_subscriber`: A opens the pair, B subs `ch:`, A sends, B receives `channels.message` seq 1. Real socket, in CI. |
| Concurrent-seq test green 100 consecutive runs | **PASS** — `concurrent_senders_gapless` (16 tasks × 50 → gapless dup-free 1..=800) run 100×: **0 failures**. |
| p99 `channels.send` < 5 ms at 30 msg/s (record it) | **PASS** — store-path p99 = **1.8 ms** (p50 1.4, max 9.3), unloaded floor over 2000 sends (`send_latency_p99`, `#[ignore]`). Paced/loaded version is Sprint 4's loadgen. |
| All `channels.*` / `notify.*` in the coverage match-test | **PASS** — `tests/coverage.rs` extended; both new `Evt` too. |
| RLS on all new tables, cross-world proof | **PASS** — `cross_world_channel_isolated`, `inbox_rls_isolated`; every domain query through `world_tx`. |

Clippy `-D warnings` clean, fmt clean, full suite green against the live stack.
CI-on-a-remote still open (no push this session).

### Reflection

- **The subagent recipe scaled again, now with a review leg.** Main thread kept
  the coupled/subtle core (both migrations, the send hot path, all contracts +
  wiring, the seq invariants); two opus agents wrote the two independent test
  suites (notify, channels breadth) against the compiling code; a third did a
  read-only adversarial review. Zero merge conflicts — disjoint file ownership
  (agents own exactly one `tests/*.rs` each; nobody touches `common/`). Writing
  the production code main-thread and delegating the *test* suites (rather than
  the reverse) fit this sprint's tight coupling better and gave the tests a
  mild independence check for free.
- **The independence paid off twice.** Agent B (channels tests) caught a real
  regression I introduced — the Sprint-2 `ws::sub_authz` still asserted the
  placeholder `not_found` for `ch:` subs, now correctly `forbidden`. The review
  agent found one real MEDIUM (the `pg_temp` search_path hole) — defense-in-
  depth in exactly the hardening I'd attempted.
- **The send hot path passed all 6 invariants on the first run.** The post-lock
  idempotency design (decision 1) was right the first time; the value was in
  reasoning it through *before* coding, not in iterating. Worth repeating: the
  subtle concurrency piece is where main-thread attention earns its keep.
- **Real-Postgres-catches-desk-checks, third sprint running.** My `pg_temp` fix
  was itself subtly wrong (`pg_catalog` first → catalog became the CREATE
  target); one test run surfaced it. The pattern is now a law of this codebase.
- **Not committed** — Sprint 3 left in the tree (Sprints 0–2 are committed;
  `feat: implemented sprint 1 and 2`). Committing + first push (which arms CI +
  the contracts drift gate, still open from Sprint 0) remains the operator's
  call.

### Next session

1. Consider committing Sprint 3 (and the first push — it burns down Sprint 0's
   last CI/drift-gate criterion).
2. Sprint 4 — channels complete + pagination + loadgen v0: the shared cursor
   util (`infra/cursor.rs`) and retrofit the inbox `?limit` read onto it
   (closes the Sprint 3 TODO); **`reactions` + `channel_pins` tables land here**
   with their handlers; receipts (watermark), typing (ephemeral), members,
   `channels.member`; **resume replay** (the `ch:` `last_seq` is accepted-and-
   ignored today — wire the >seq replay before the sub ack, overflow event at
   500); history HTTP (`JwtIdentity` extractor is ready); and `opn-loadgen` v0
   + the nightly perf smoke. Remember: online-member badging (decision 9) and
   the Bearer-scheme case-sensitivity (a codebase-wide minor, shared with
   `TenantAuth`) are open if they matter.

---

## 2026-07-18 (later still) — Sprint 2 (WS gateway): built, all exit criteria pass

### What exists now

- **`gateway/`** — the new module, six files:
  - `ws.rs` — full connection lifecycle (§4.1): two-phase origin check
    (pre-upgrade against the cached *union* of all tenants' origins + NUI
    prefixes; authoritative per-tenant re-check post-auth), pre-auth caps
    (global `AtomicU32` + per-IP `DashMap`, guard struct releases on every
    path via `Drop`, over-cap → HTTP 429 pre-upgrade), auth-in-3s (else
    4401; non-auth first frame 4400), reader/writer split, heartbeat
    (ping every `OPN_HEARTBEAT_SECS`, close 1001 after 2 missed pongs).
  - `registry.rs` — real `SessionRegistry`: `DashMap` sessions/topics,
    per-`Evt::class()` backpressure (durable full → close 4409; ephemeral
    dropped below ~20 % headroom), presence connection counts, close codes
    module (4400/4401/4408/4409).
  - `dispatch.rs` — sequential per-connection dispatch (CDR-5): rate limit
    → span + latency histogram → handler → ack; `internal` acks carry no
    detail (§7); identity handlers from Sprint 1 wired in; `auth.refresh`
    (guarded UPDATE re-checks revocation, bumps expiry, fresh JWT).
  - `presence.rs` — Redis `SET presence:<world>:<char> EX 90`, pipelined
    refresh task per heartbeat tick (one pass over local sessions, no
    per-conn round trips), snapshot-on-sub *before* the sub ack,
    online/offline transitions only on the 0↔1 connection edge.
  - `topic.rs` — typed `TopicKind` parse; `ch:`/`call:`/`feed:` sub acks
    `not_found` until their primitives land; `notify:` own-device-only.
  - `fanout.rs` — Redis pub/sub cross-replica fan-out behind
    `OPN_REPLICAS > 1`: channel `opn:<world>:<topic>`, payload
    `{from: replica_id, evt}`, self-drop on the origin replica,
    reconnect-forever listener. Tested with two in-process replicas
    sharing one Redis, per the roadmap's warning.
- **`infra/ratelimit.rs`** — lazy token bucket per §12 table (msg/social/
  money/expensive/read), `class_of(&Cmd)` exhaustive (no wildcard — the
  compiler forces every new command to pick a bucket), janitor sweeps
  buckets idle > 10 min.
- **Contracts** — `Cmd::Auth`, first `Evt` variant (`presence.state`,
  ephemeral), `ServerMsg::Push` now really flattens `Evt` in TS (Sprint 0's
  `ts(skip)` ponytail note paid off), golden wire tests extended, bindings
  regenerated.
- **Protocol harness** (`tests/common/ws.rs`) — `spawn_server` (real
  listener + `connect_info`), `connect_and_auth`, `cmd()` (buffers pushes
  seen before the ack), `expect_evt`/`expect_no_evt`/`expect_close`
  (ping-skipping), `mint_token`, `hold_until_close`. The backbone the
  roadmap asked to invest in.
- **Tests** — 65 green across the workspace: 12 WS lifecycle/protocol tests
  (auth paths, all four close codes, takeover, missed pongs, bad-JSON-
  never-closes, sub authz, presence snapshot + transitions + null-snapshot,
  rate-limited ack with `retry_after_ms`, auth.refresh incl. revocation),
  registry unit tests (backpressure both classes, takeover-cleanup race,
  unsubscribe), token bucket, topic parse, two-replica fan-out + self-drop,
  and the `Cmd`/`Evt` coverage match-tests (cross-cutting rule 2, deferred
  from Sprint 0, now real). Plus `tests/idle_soak.rs` (`#[ignore]`): the
  300-connection RSS soak.

### Decisions closed during implementation

1. **Topic entries are `(session_id, conn_seq)` pairs, not bare session
   ids.** Takeover (4408) replaces the handle under the *same* session id;
   with bare ids, the old connection's cleanup could strip a subscription
   the successor just made (subscribe-before-old-cleanup race). A
   process-wide `conn_seq` counter disambiguates; stale pairs prune lazily
   on publish. Unit test `takeover_cleanup_preserves_successor` pins it.
2. **Close signaling is a `watch` channel, not task aborts.** Reader and
   writer both `select!` on it; first close code wins (`send_if_modified`).
   The writer can therefore always send the close *frame* (4408/4409) even
   when the send queue is full — an aborted task can't.
3. **Acks use durable-class semantics**: a queue too full to take the reply
   the client is waiting on is the same slow consumer as a full durable
   event queue → 4409.
4. **Ephemeral headroom generalized** from the roadmap's `capacity() < 52`
   (at 256) to `capacity() < cap/5 + 1` — queue depth is now config
   (`OPN_SENDQ_CAPACITY`) so backpressure tests don't need thousands of
   events.
5. **Absent `Origin` header is allowed** in both phases (loadgen, native
   shells, tests — browsers always send it); NUI = `https://cfx-nui-*` or
   `nui://*` prefixes. Union check added to `TenantCache` (60 s TTL like
   the per-tenant entries).
6. **Presence is per-(world, character) connection *counts*** — a character
   can hold several device sessions; transitions fire only on the 0↔1
   edge, `last_seen_at` written on every disconnect regardless.
7. **`share_presence` rides the `ConnHandle`** as an `AtomicBool` (read at
   auth, updated live by `identity.set_share_presence` on that session) —
   the emit-time check without a DB read; snapshot path reads the DB fresh.
8. **Fan-out replica id = process-random UUID ⊕ registry `Arc` pointer**
   (subagent's catch): the spec's process-global id and the roadmap's
   two-replicas-in-one-process test are mutually exclusive — a shared
   `OnceLock` would make replica B self-drop A's messages. The XOR keeps
   cross-process uniqueness from the random part and disambiguates
   in-process replicas; production (one `AppState` per process) unaffected.
9. Rate classes for Sprint 2 commands: reads + `sub`/`unsub`/`auth*` →
   `read`; `set_settings`/`set_share_presence`/`app_login` → `social`.
10. `sub { last_seq }` accepted and ignored-with-log (protocol shape
    stable; replay lands Sprint 4). Repeated `auth` post-auth → `conflict`.

### Exit criteria status

| Criterion | Status |
|---|---|
| Harness merged, used by every gateway test | **PASS** — all 12 `tests/ws.rs` tests + fan-out + soak ride `common::ws`. |
| `Cmd` coverage match-test exists and fails on an uncovered variant | **PASS** — `tests/coverage.rs`, exhaustive match, no wildcard; `Evt` too. |
| 300 idle authenticated connections, RSS steady over 10 min | **PASS** — `idle_soak` test: RSS 99 368 kB after 60 s settle, 99 368 kB after the 9-min window (growth **0 kB**), zero connections dropped. Kept as `#[ignore]` for re-runs; automated properly in Sprint 10. |

Clippy `-D warnings` clean, fmt clean, full suite green against the live
stack. CI-on-a-remote remains open (no push yet — operator's call), so the
drift gate is still unarmed; bindings are regenerated and committed-ready.

### Reflection

- **Three opus agents, same recipe as Sprint 1** (tight spec, hard file
  ownership, stub files pre-created): rate limiter, fan-out + two-replica
  test, harness + 11 lifecycle tests. Zero merge conflicts again. The
  harness agent found *zero* gateway bugs — the main-thread lifecycle code
  survived contact with 11 adversarial tests untouched, which suggests the
  subtle-parts-stay-main-thread split is calibrated about right.
- **An agent caught a real spec contradiction** (decision 8): the fan-out
  spec I wrote was unimplementable as stated against its own required test.
  The agent identified the conflict, resolved it correctly, and documented
  why — that's a step above Sprint 1's "resolved a library constraint".
  Worth keeping the instruction pattern that made it possible: "if the spec
  conflicts with reality, deviate and explain, don't silently comply".
- **Agent output still needs the main-thread clippy gate**: two trivial
  misses this time (`unwrap_err` under `clippy::unwrap_used`, dead-code
  allows for per-binary test helper subsets). Cheap to fix, but the gate
  stays.
- **The watch-channel close design (decision 2) is the sprint's keeper**:
  every "how does X interrupt Y" question (takeover, slow consumer,
  heartbeat death, normal EOF) collapsed into one mechanism with one rule
  (first code wins). No aborts, no lost close frames, no cleanup races.
- **Not committed** — Sprint 0+1+2 work all still untracked; committing and
  first push (which arms CI + the drift gate) remain the operator's call,
  and the pile is getting tall.

### Next session

1. Strongly consider the first commit + push: three sprints of uncommitted
   work is real risk, and Sprint 0's CI criterion plus the drift gate stay
   open until it happens.
2. Sprint 3 — Notify + channels hot path: `inbox` + `notify::route`
   (class chosen by caller, muted → silent), channels schema with
   partitioned `messages` from migration one (+ janitor next-month-partition
   stopgap — remember the worlds-loop RLS lesson from Sprint 1), the send
   hot path (per-channel seq via row lock, persist-then-ack, post-commit
   fan-out through `gateway::publish`), `open_direct` (ordered-pair unique
   columns), `create`/`list`, `ch:*` sub authorization, cross-partition
   idempotency dedup. The concurrent-seq test (16×50 → contiguous 1..=800)
   is the sprint's named bug magnet — write it early.

---

## 2026-07-18 (later) — Sprint 1 (Identity & auth): built, all exit criteria pass

### What exists now

- **Migration `0003_identity.sql`** — all seven tables (§10.1) plus
  `sessions.app_accounts jsonb` (per-session active app account) and the
  `sessions (expires_at)` index. The five world-scoped tables get the 0001
  RLS convention applied by one `DO $$ FOREACH` loop (one place to get the
  NULLIF form right instead of five). `worlds`/`tenants` are *not* RLS'd per
  the roadmap exception — `opn_app` gets column-level SELECT only
  (`tenants`: id/name/api_key_hash/allowed_origins/world_id), which is what
  lets API-key auth run as the app role before any world context exists.
- **`infra/auth.rs`** — `api_key_hash()` (sha256 hex), `mint_jwt()`/`verify()`
  (HS256, claims `sid/tenant/world/char/device/exp`, 10 min const), and
  `Identity` whose only constructors are `verify()` and the mint path
  (`_priv: ()` field — see decisions).
- **`primitives/identity.rs`** — `mint_session` (upsert character →
  number assignment → device resolve/create → session insert, one
  `world_tx`), `get_settings`/`set_settings` (whole-doc replace, 16 KB cap),
  `set_share_presence`, `me`, `app_login`. Handlers are plain async fns
  returning `Result<T, Fail>` (`Fail::Code(ErrCode) | Internal(anyhow)`) —
  dispatch wires them in Sprint 2.
- **`http/tenant.rs`** — `TenantAuth` extractor (Authorization header, hash
  lookup), `POST /v1/tenants/self/sessions`, and the ErrCode→HTTP status
  mapping every future route reuses.
- **`janitor.rs`** — 30 s tick loop, per-task span + `opn_janitor_runs_total`
  metric, failure never kills the loop; tasks `expired_sessions` (7-day
  grace) and `retired_numbers_sweep` (30-day cooldown expiry).
- **`infra/tenant_cache.rs`** — hand-rolled 60 s TTL `RwLock<HashMap>` (~40
  lines, no moka), wired into `AppState`; first consumer is Sprint 2's
  origin check.
- **`admin.rs` + main wiring** — `opn-core admin create-tenant --name X
  (--world <uuid> | --new-world <name>)`, owner-role connection, key printed
  once. README documents the full mint flow.
- **Contracts** — `Cmd` grew the five `identity.*` variants +
  `SettingsScope`; `types.rs` with `SessionMintResponse`/`MePayload`/info
  structs; bindings regenerated.
- **Tests** — 38 green against the live stack: upsert idempotency,
  32-concurrent-mint distinctness, cooldown exclusion (all 10 k numbers
  retired → mint fails `internal` at the 10-attempt cap), JWT
  tampered/expired/revoked, settings roundtrip + size cap, me/app_login
  authz, mint-over-HTTP happy/401 via `oneshot`, per-table cross-world RLS
  isolation, three janitor tests. Shared `tests/common/mod.rs` now holds the
  `app_pool` + world/tenant seeding helpers (the extraction Sprint 0's
  reflection predicted).

### Decisions closed during implementation

1. **Wire names are dotted, per design doc.** Sprint 0 shipped
   `auth_refresh` (enum-level `rename_all`); OPN-CORE.md §10.1 says
   `auth.refresh`. Design-doc-wins applied: explicit
   `#[serde(rename = "auth.refresh")]` per variant (`sub`/`unsub` stay
   bare), golden tests updated. Caught before any client existed — cheap
   now, breaking later.
2. **Number-assignment retries need SAVEPOINTs.** A unique-violation on the
   conditional `UPDATE` aborts the *enclosing* Postgres transaction; the
   roadmap's "retry with a fresh candidate" is impossible in one tx without
   `SAVEPOINT`/`ROLLBACK TO` around each attempt. ~4 lines, documented in
   `assign_number`. Zero-row update disambiguates via re-select (concurrent
   mint won vs candidate-in-cooldown).
3. **Mint without `device_id` reuses the character's first device** instead
   of inserting one per login (roadmap silent; a device row per session
   would be nonsense for "pure hardware" rows).
4. **`Identity` privacy via `_priv: ()`** rather than `#[non_exhaustive]`
   (which only gates *other* crates — the invariant is intra-crate).
   Clippy's `manual_non_exhaustive` allowed with a comment saying why.
5. **Number pattern hardcoded `555-XXXX`** (10 k/world) with a `ponytail:`
   note — worlds get a pattern column when a deployment needs it.
6. **Janitor iterates worlds.** FORCE RLS means a pool-direct `DELETE
   FROM sessions` silently deletes nothing — every sweep is
   `SELECT id FROM worlds` then per-world `world_tx` + advisory lock. This
   is the second RLS behavior (after Sprint 0's empty-string GUC) that a
   desk-check would have shipped broken.
7. **sqlx 0.9 requires `'static` SQL strings** (`SqlSafeStr`): no
   `&format!(...)` queries. Shaped the janitor helper (static DELETE
   literals as params) and the RLS isolation test (literal table list).
8. Settings cap enforced as serialized-JSON bytes in Rust (≤ 16 KB), not
   `pg_column_size` — same effect, no extra round trip.

### Exit criteria status

| Criterion | Status |
|---|---|
| `curl` from README mints a real session | **PASS** — ran live: create-tenant CLI → boot → mint returned token + `555-4133`, bad key → 401. |
| Concurrent-mint test 100 consecutive runs | **PASS** — 100/100 green (scripted loop, per-run fresh DB). |
| RLS policies on all new tables + cross-world proof | **PASS** — `all_identity_tables_are_world_isolated` seeds a full graph in world A, world B sees zero rows in all five tables. |

Clippy `-D warnings` clean, fmt clean. Sprint 0's remaining "CI green on a
remote" criterion is still open (no push yet — operator's call).

### Reflection

- **Subagent split worked better than Sprint 0.** Three opus agents in
  parallel (janitor + tests, tenant cache + admin CLI, contracts commands +
  bindings) with tight specs and hard file-ownership boundaries (stub files
  created first, "don't touch lib.rs/main.rs/Cargo.toml"). Zero merge
  conflicts, zero rewrites needed; the janitor agent even resolved the
  sqlx-0.9 `'static` constraint on its own and reported it. Main thread kept
  the four subtle pieces: migration/RLS grants, number assignment, JWT/
  `Identity`, and the concurrent-mint test — right split, would repeat.
- **Two compile-time catches from the review pass**: `SettingsScope` needed
  `Copy` (test ergonomics), and clippy flagged the `_priv` idiom — both
  trivial, but they confirm agents' output still needs a main-thread
  compile+clippy gate before anything else builds on it.
- **The RLS/janitor interaction (decision 6) is the sprint's lesson**: any
  future cross-world maintenance code (partition creation, media sweeps,
  reconciliation) must start from the worlds loop, not the pool. Worth
  remembering when Sprint 3's partition-stopgap janitor task lands.
- **Not committed** — Sprint 1 work left in the tree; committing remains the
  operator's call.

### Next session

1. Optionally: push + CI (burns down Sprint 0's last criterion and arms the
   drift gate for the contracts changes made here).
2. Sprint 2 — WS gateway: connection lifecycle (auth-in-3s, takeover,
   pre-auth caps), real `SessionRegistry` (DashMap), writer/backpressure
   with `Evt::class()`, sequential dispatch, token-bucket rate limiting,
   sub/unsub + presence, `auth.refresh` handler, Redis fan-out listener,
   and the protocol harness (`connect_and_auth`/`cmd`/`expect_evt`) — plus
   the `Cmd`/`Evt` coverage match-test deferred from Sprint 0. The identity
   command handlers written this sprint get wired to dispatch there.

---

## 2026-07-18 — Sprint 0 (Foundations): built, compiles, partially verified

### What exists now

Workspace at `opn-core/` (repo root stays docs-only):

- **`crates/contracts`** — `ClientFrame` (flattened adjacently-tagged `Cmd`:
  `{id, cmd, payload}`), `ServerMsg` (untagged `Ack`/`Push`), closed 8-variant
  `ErrCode` + `Err` body, empty `Evt` with `class() -> EvtClass`
  (`Durable|Ephemeral`) as an exhaustive match — the compile-time forcing
  function for backpressure class is live from day one. `bin/export_ts`
  writes `bindings/*.ts` (committed; ts-rs 11).
- **`crates/core`** — lib + thin `main`. `Config::from_env()` (fail-fast,
  names the missing var), `AppState` with registry/limits stubs,
  `infra::ids::new_id()` (UUIDv7), `infra::db::world_tx()` (BEGIN +
  parameterized `set_config('app.world_id', …, true)`), `/healthz`
  (PG + Redis, 1 s timeouts, 503 on either failing), `/metrics` on the
  separate bind, all eight roadmap metric names registered at boot,
  JSON tracing. Migrations `0001` (roles + RLS convention doc) and `0002`
  (`_rls_canary` table proving the full RLS pattern).
- **`crates/loadgen`** — placeholder member per roadmap item 1.
- **Dev stack** — `docker-compose.dev.yml` (PG16/Redis7/MinIO+bucket
  one-shot), `justfile`, `.env.example`, `README.md` quickstart.
- **CI** (`.github/workflows/ci.yml`) — fmt, clippy `-D warnings`, tests
  against the compose stack, contracts drift gate
  (`export_ts` + `git diff --exit-code`), `cargo sqlx prepare --check`.
- **Tests** — golden wire-shape round-trips (literal JSON strings, compared
  as `Value`), config missing-var/default test, RLS canary
  (`#[sqlx::test]`, seeds two worlds as `opn_app`, asserts: bare query → 0
  rows, `world_tx` → own world only, cross-world insert rejected by
  WITH CHECK), `/healthz` 200/503 via `tower::oneshot`.

Toolchain pinned 1.97.0; `clippy::unwrap_used` denied workspace-wide;
resolved deps: axum 0.8.9, sqlx 0.9.0, redis 1.4.1, metrics 0.24,
ts-rs 11.1.

### Decisions closed during implementation (not in the roadmap)

1. **Two database URLs.** The roadmap wants migrations run by the owner role
   and the runtime pool as non-BYPASSRLS `opn_app`, but never says how one
   process does both. Closed: `OPN_MIGRATE_DATABASE_URL` (owner, used once
   at startup, pool closed after) + `DATABASE_URL` (app role, the 20-conn
   runtime pool). `.env.example` documents both.
2. **RLS policies use `current_setting('app.world_id', true)`** (missing_ok
   form): outside `world_tx` a query returns zero rows instead of erroring.
   The bare form would error on *every* query outside a world context,
   including legitimate infra reads. Documented in 0001's header.
3. **`CREATE ROLE` is guarded** (existence check + `duplicate_object`
   handler) because roles are cluster-wide while migrations run per-database
   — `#[sqlx::test]` creates a DB per test, concurrently. Without the
   handler, parallel test DBs race the existence check.
4. **`Err` struct is Rust-named `ErrBody`** (`#[ts(rename = "Err")]`), so
   the TS surface matches the design doc without shadowing Rust's prelude.
5. **`u64`/`i64` wire integers export as TS `number`, not `bigint`**
   (`#[ts(type = …)]` overrides) — the wire is JSON numbers; frame ids and
   seqs never approach 2^53.
6. **ts-rs cannot flatten an empty enum**: `ServerMsg::Push.evt` carries
   `#[ts(skip)]` with a `ponytail:` note; swaps to `#[ts(flatten)]` when the
   first `Evt` variant lands (Sprint 2). Until then a Push cannot exist on
   the wire, so the TS type is accurate in effect.
7. **`sqlx::test` needs CREATEDB**, so test runs use the migrate role
   (`just test` overrides `DATABASE_URL`; CI sets it directly). RLS-on
   testing (cross-cutting rule 4) is still honored: the canary test opens a
   *second* pool as `opn_app` against the per-test DB and runs all
   assertions through it. Store tests from Sprint 1 on must follow this
   pattern — worth extracting a small test-support helper when the second
   test needs it.

### Exit criteria status (updated same day, after docker install)

| Criterion | Status |
|---|---|
| Compose up → `cargo run` → 200 `/healthz` | **PASS** — stack up (postgres/redis/minio healthy, bucket created), binary boots, migrations apply, `/healthz` 200. |
| CI green, drift gate demonstrably fails on uncommitted change | **Not yet** — repo has no remote/first push. Workflow file ready. |
| RLS canary passes as `opn_app` | **PASS** (after a real bug fix — see below). |
| Metric names visible on `/metrics` | **PASS** — all eight `opn_*` families render at boot. |

Full suite green against the live stack: 16 tests, clippy zero warnings.

### Bug found by running against real Postgres (the whole point of rule 4)

The RLS policy used `current_setting('app.world_id', true)::uuid`, believing
the two-arg form's NULL-when-unset covers the no-context case. It does — but
only until any transaction on that *pooled connection* has run
`set_config(..., true)`: at commit the GUC reverts to an **empty string**,
not to "unset", and `''::uuid` then throws `22P02` on every subsequent
bare query on that connection. The canary test hit it immediately (seed via
`world_tx`, then bare query on the same pool). Fix:
`NULLIF(current_setting('app.world_id', true), '')::uuid` — now part of the
documented convention in migration 0001 (NOTE 2). Every future domain-table
policy must use the NULLIF form. A desk-checked migration and a green
compile both missed this; only the live-DB test caught it.

### Reflection

- **The environment gap is the sprint's real blocker, not code.** Everything
  DB-touching is written test-first but unproven. Before starting Sprint 1
  (which is *mostly* DB semantics — number assignment races, session
  revocation), either install docker/podman on this host or push to GitHub
  and let CI be the proving ground. Recommend doing both; Sprint 1's
  concurrent-mint test is exactly the kind that needs fast local iteration.
- **Subagent split worked**: compose/justfile/env, CI workflow, and
  migration SQL went to three parallel opus agents with tight specs; all
  three delivered usable output. Two agent bugs caught in review — CI passed
  a file path to the toolchain action's `toolchain:` input (invalid), and
  `up -d --wait` would fail on the one-shot bucket container exiting. Both
  fixed by naming services and letting rustup read the toolchain pin.
  Lesson: agents are fine for scaffolding, but CI files need review against
  actual action semantics, and anything touching *startup ordering* deserves
  main-thread eyes.
- **Deferred, deliberately**: the `Cmd`/`Evt` coverage match-test
  (cross-cutting rule 2) — with zero handlers, every arm would name a
  placeholder, making the test decorative. It lands with the protocol
  harness in Sprint 2 where the roadmap's own exit criteria place it. The
  ts-rs 12 upgrade (11.1 resolved; 12.0.1 exists) — pinned where cargo put
  it, upgrade when something needs it.
- **Not committed**: work left as untracked files; committing is the
  operator's call.

### Next session

1. Get a container runtime (or push + CI) and burn down the four unverified
   exit criteria — Sprint 0 is *not done* until they pass, per the
   scope-bound rule.
2. Then Sprint 1: schema migration for the seven identity tables (RLS per
   table, same file), API-key auth + `TenantAuth` extractor, session mint
   with the number-assignment retry loop, JWT + `Identity`, janitor v0
   skeleton. The concurrent-mint test (32 tasks, one world) is the sprint's
   named bug magnet — write it before the mint handler.
