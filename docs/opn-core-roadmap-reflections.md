# OPN-CORE Roadmap — Implementation Reflections

Running log, one section per work session. Newest first. Companion to
[opn-core-roadmap.md](opn-core-roadmap.md); design-level amendments still go
to OPN-CORE.md as CDRs — this file records *how the build actually went*.

---

## 2026-07-18 (later) — Sprint 6 **part A** (calls: schema, FSM-as-data, start/accept/decline/hangup/signal, snapshot-on-sub, ring-via-notify, zombie reap): built, verified live; tenant link (part B) deferred

Same seam-split as every sprint since 4: Sprint 6 has two shared-nothing halves —
**(A)** the WS-facing call primitive (session FSM, the WebRTC signaling relay,
ring delivery, janitor reap) and **(B)** the tenant `/link` gateway (voice-target
`set_targets`/`clear` events, `/calls/active` re-sync, coturn + `ice_servers`).
A touches only `call_sessions`/`call_participants` + the gateway; B is a separate
connection type and registry. So A shipped as a complete, reviewed, green slice
and stopped here. No roadmap amendment (items 6/7 stay in Sprint 6).

### What exists now

- **Migration `0009_calls.sql`** — `call_sessions` (kind, state, ended_at) +
  `call_participants` (state, device_id, joined_at/left_at, PK `(call_id,
  character_id)`). Two partial indexes: `call_participants_active`
  `WHERE state IN ('ringing','joined')` (the busy check) and
  `call_sessions_active_age` `WHERE state <> 'ended'`. Standard 0001 NULLIF
  FORCE-RLS on both, grants to `opn_app`.
- **`primitives/calls/fsm.rs`** — the state machine as **one pure function**
  `apply(session, actor, others, action) -> Result<Transition, ()>` over the
  contracts enums (no duplicate enum, no conversion at the store boundary).
  Accept→Joined/Active, Decline→Declined (+ end iff no other Ringing|Joined),
  Hangup→Left (+ end iff no other Joined = last-hangup), `Ended` absorbs
  everything. The Sprint 9 proptest target; 6 unit tests cover the table + terminal
  absorption.
- **`primitives/calls/store.rs`** — `start` (resolve via the directory seam →
  block/unknown → `NotFound`; busy callee → `Conflict`; caller `joined` + callee
  `ringing` in one tx), `transition` (session `FOR UPDATE` then participants
  id-ordered `FOR UPDATE` → deadlock-free; run the pure FSM; persist; return the
  fresh snapshot), `authorize_sub` + `snapshot` (split for subscribe-first),
  `authorize_signal`, `reap_zombie_rings`.
- **`primitives/calls/mod.rs`** — handlers + fan-out: `start` rings the callee via
  `notify::route(class=ring, app_id="dialer")` (best-effort; the reap backstops an
  unanswered ring), accept/decline/hangup publish the full `calls.state` snapshot
  on `call:<id>`, `signal` authorizes both parties then relays `calls.signal`
  verbatim (never stored/inspected, 16 KB cap checked before any DB work).
- **Contracts** — 5 `Cmd` (`calls.start/accept/decline/hangup/signal`), 2 `Evt`
  (`calls.state` full snapshot, `calls.signal` relay — both **Durable**), 4 types
  (`CallKind`, `CallSessionState`, `CallParticipantState`, `CallParticipant`).
  Bindings regenerated (drift gate armed with 4 new `.ts` + updated Cmd/Evt).
- **Wiring** — dispatch (5 command arms + the `sub call:<id>` snapshot-on-sub arm,
  which the Sprint 2 stub returned `not_found` for), `wire_name`, rate classes
  (all `calls.*` → `Social`), janitor `calls_reap` task, coverage match-test
  (5 Cmd + 2 Evt), golden wire tests (5 commands + the two pushes).

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **`ice_servers`/coturn deferred to part B, no wire change in A.** The design
   puts `ice_servers` in the `calls.state` snapshot, sourced from coturn config —
   which is the part-B "everything video needs from the backend" bucket. Part A's
   snapshot ships without it; a client can drive the FSM/signaling relay in tests
   without a TURN server, and adding the field in B is additive (no golden churn
   for A).
2. **Signaling relay = publish-on-`call:<id>` with `from`/`to`, clients filter.**
   Reuses `gateway::publish` (so it crosses replicas for free), 1:1-safe (a call
   has exactly two participants, so `from`/`to` fully partition the topic).
   Directed per-recipient routing is the upgrade **if** group calls ever exist —
   marked at the fan-out site.
3. **FSM uses the contracts enums directly**, not a private core copy — the store
   maps DB text ↔ enum, the handler maps enum → wire, and the pure function needs
   no conversion. One source of truth for the three state sets.
4. **`calls.*` all rate-class `Social`.** start/accept/decline/hangup are
   occasional; `signal` carries WebRTC trickle-ICE, and Social's burst-20 covers a
   setup trickle. Noted as a Sprint-10 budget-tuning candidate.
5. **Ring is best-effort with no cancel-notify.** `start` rings via notify;
   nothing pushes a *cancellation* to a callee who hasn't yet subscribed to
   `call:<id>` — a stale-accept just gets `Conflict`. Inherent to the "dialer
   needs no standing sub" design (§10.4).

### The keeper this session (the point of rule 4): a HIGH bug caught by **triple convergence**

The independent test author **and both adversarial reviewers** independently
landed on the same defect: **`reap_zombie_rings` was dead code.** My reap keyed on
"non-ended session with **no `joined` participant**" (straight from the design's
§10.4 wording). But `calls.start` joins the **caller** immediately, and a WS
disconnect never reconciles call participant rows (confirmed: `ws.rs` cleanup only
touches presence/registry) — so a crashed caller stays `joined`, every real ring
keeps a joined participant, and the `NOT EXISTS(joined)` guard is **false for every
ring the reap was written to catch**. Consequence is a griefing DoS: start a call,
kill the socket, and the victim's participant row stays `ringing` forever → the
busy check pins them **permanently un-callable**. The reap only ever fired on
artificially-seeded rows.

This is the design's own predicate being *unimplementable* given caller-joins-at-
start. Fixes: (a) the reap now keys on `state = 'ringing' AND created_at < now() -
60s` — a ring only leaves `ringing` via accept, so this reaps exactly the
un-accepted ones and never an `active` call; (b) **OPN-CORE.md §10.4 amended**
(design-doc-first) with the dated rationale so nobody re-derives the broken
predicate from the doc. The test author's `#[ignore]`d repro (real `calls::start`
+ aged `created_at`) is now un-ignored and green; the reap's own happy-path test
had encoded the buggy "joined-spared" semantics (a *ringing*+joined session must
now be reaped) and was corrected to spare an **active** call instead.

Lesson, fourth sprint running: the independent test leg catches a shipped defect
every time — and here it converged with two adversarial reviewers on the *same*
line, which is the budgeted-verification story (ADR-1) working exactly as designed.
A desk-check reads "no joined participants → reap" as obviously correct; only
tracing `start` → disconnect → the busy check end-to-end exposes that it can never
fire.

### Also fixed (MED, both reviewers): the snapshot-vs-live race on `sub call:`

`calls.state` is a durable **full-state** event with no seq to heal a lost
update. The original sub arm copied presence's `compute → subscribe → push`, but
presence is ephemeral/self-healing and calls are not — a transition landing in the
window is lost, and a lost *terminal* snapshot leaves a permanent ghost call UI.
Switched to the **durable idiom** (the `ch:` arm's order): `authorize → subscribe
→ read snapshot → push`, so a post-registration transition is delivered, never
missed. The residual reorder window (a stale snapshot arriving just after a newer
live event — a transient the next transition heals, and terminal states are
sticky client-side) is documented at the call site with a monotonic `version` on
`calls.state` named as the full close — deferred, since §10.4 deliberately chose
seqless snapshots.

### Documented, not fixed (LOW — acceptable for the v1 1:1 dialer)

- **Group-signaling privacy**: publish-on-topic would show a third participant
  A→B's signaling — moot at two participants; noted for any future group call.
- **Busy-check TOCTOU**: no callee lock/constraint, so two simultaneous dials of
  one callee both ring; blast radius is a duplicate ring that now reaps in 60 s.
- **`authorize_signal` state split**: a non-participant can tell ended/active/
  missing apart — moot, call ids are unguessable v7 uuids.
- **Online-ring race**: `notify::route` live-pushes an online callee without an
  inbox fallback (a callee racing offline drops the ring) — a shared `notify`
  property already noted, not calls-specific.

### Verification (rule 4)

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean (the
  `core` crate's `unwrap_used` deny respected — `.expect`/`assert!`/`?` only).
- **Full workspace suite green** against the live stack (Postgres+Redis+MinIO):
  every binary passes, 0 failures. New `tests/calls.rs` (10 tests): the WS-wire
  full lifecycle (start → ring → snapshot-on-sub → accept → signal relay →
  hangups → ended), start rejections (self/unknown/busy/blocked byte-identical),
  decline's end-rule, signal authz + 16 KB cap + ended→conflict, FSM conflict
  paths, participant-only sub, the janitor reap (+ the un-ignored crash regression),
  cross-world RLS, and a concurrent-hangup deadlock canary. Plus 6 `fsm.rs` units
  and 3 golden wire tests.
- Both adversarial reviewers confirmed FSM faithfulness (cell-by-cell), SQL bind
  correctness, deadlock-free lock ordering, RLS/migration correctness, and wiring
  completeness — beyond the one HIGH + one MED they found.

### Exit criteria status (Sprint 6 — part A slice)

| Criterion | Status |
|---|---|
| Scripted two-client + fake-link demo (call connects, link `set_targets`, hangup clears) | **PARTIAL** — the two-client call lifecycle is `full_lifecycle_start_accept_signal_hangup` (real sockets); the fake-**link** half is part B. |
| FSM is a pure function with 100 % transition-table coverage | **PASS (part A scope)** — pure `apply`, unit tests over the table + terminal absorption; the exhaustive generated proptest is Sprint 9. |
| All `calls.*` in coverage test; `/link` + re-sync in route test | **PARTIAL** — all 5 `calls.*` + both events in the coverage match-test; `/link` is part B (adds no HTTP route in A). |

### Reflection

- **The seam-split paid a fourth time.** Calls-core and the tenant link share
  nothing, so part A is a complete reviewed slice and B starts clean.
- **Triple convergence on the reap bug is the verification thesis in miniature.**
  One independent test author + two lensed reviewers, all three on the same
  unimplementable-design-predicate — exactly the budgeted work ADR-1 buys, not
  polish.
- **Design-doc-first held under a real deviation.** The §10.4 predicate was wrong;
  amended the design (dated) before trusting the code, per the standing rule.
- **Not committed.** Sprint 6A sits untracked on top of the committed 0–5. First
  commit + the drift-gate re-arm (4 new binding files) is the operator's call.

### Next session

1. **Sprint 6 part B — tenant link.** `GET /link` WS (API-key auth, last-writer
   takeover), hello handshake, **down-only** `calls.voice { set_targets|clear }`
   emitted from the accept/end handlers (the hook sites are marked in
   `calls/mod.rs`), `GET /v1/tenants/self/calls/active` re-sync, coturn in compose
   + `ice_servers` in the `calls.state` snapshot.
2. **The deferred `calls.state` `version`** — close the snapshot-vs-live residual
   if part B's link work touches the snapshot shape anyway.
3. Still open, minor: online-member badging, Bearer case-sensitivity,
   `identity.me` own-last-seen — all pre-existing, none blocking.

---

## 2026-07-18 (late night) — Sprint 5 **part B** (directory: contacts, blocks, opaque resolve, block enforcement at open_direct, listings + cursor + janitor expiry): built, verified live, all Sprint 5 exit criteria now closed

Part A stopped on the media/directory table seam; part B is the other half.
The two share no tables and no code, so part B started from the green part-A
base and lands the whole directory primitive. With it, **Sprint 5 is complete**
— every exit criterion passes.

### What exists now

- **Migration `0008_directory.sql`** — three FORCE-RLS world-scoped tables:
  `contacts` (PK `(owner_character, number)`), `blocks` (PK
  `(blocker_character, blocked_number)`), `listings` (id PK, app-scoped, optional
  `expires_at`). Two listing indexes: the feed keyset
  `(world_id, app_id, created_at DESC, id DESC)` and a partial expiry index.
- **`primitives/directory/mod.rs`** — rewritten from the Sprint-3 resolve stub
  into the full primitive (SQL inline, media-style). Ten commands:
  `contact_upsert` / `contact_delete` / `contacts`, `block` / `unblock` /
  `blocks`, `resolve`, `listing_create` / `listing_delete` / `listings`
  (cursor). The internal `resolve(tx, caller, number)` now filters blocked pairs
  in **both** directions (caller blocked target, or target blocked caller's own
  number) → `None`, so a block is indistinguishable from an unknown number.
- **Block enforcement live at `channels.open_direct`** (Sprint 5 item 8): the
  store's `resolve` call now passes the caller, so a blocked pair resolves to
  `None` → `NotFound`, byte-identical to no-such-number. One-line change at the
  seam; no open_direct logic moved.
- **`directory.resolve`** returns `{ reachable, number, display_name }` — never a
  character id. `display_name` is the caller's **own** saved contact label, so it
  leaks nothing about the target and is present/absent independent of whether the
  number is real. This is the roadmap's chosen "no token machinery" path (§10.7,
  item 7) made concrete.
- **`listings_expire` janitor task** on the 30 s tick (SQL-only → rides the
  shared `sweep_worlds` helper). Wired dispatch arms, rate classes (writes →
  `Social`, reads → `Read`), `wire_name`, the coverage match-test (all ten
  commands), and three new exported binding types (`ContactItem`,
  `ResolveResult`, `ListingItem`).

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **Directory is all-WS — no HTTP routes.** §10.7 names every operation a
   `directory.*` command; media/inbox split reads to HTTP, but directory did
   not, so the reads return `{ items, next_cursor }` in the ack. Fewer moving
   parts (no JWT-extractor plumbing) and it satisfies "HTTP routes in
   route-coverage test" trivially — there are none to cover.
2. **`listings.owner_character` added** — the §10.7 tuple omits an owner, but
   CRUD's delete needs one to scope authz (delete-not-yours → `NotFound`, no
   leak). A column addition, logged here rather than a CDR (same weight as
   part A's "no S3 SDK" call).
3. **Contacts unpaginated, listings cursor-paged.** A character's contact book is
   a naturally bounded set; world-wide listings are not. Only listings get the
   cursor idiom (which is also all the roadmap asked for).
4. **`avatar_media` carries no FK to `media(id)`.** This matches the
   message-attachment precedent (media ids in a message body have no FK either):
   ownership+live is validated at write time, and a later-deleted avatar just
   renders missing. An FK would instead make the media janitor's `DELETE` fail
   while a reverted/reaped row is still referenced by a contact — a self-inflicted
   foot-gun avoided. (Caught in self-review before the reviewers ran.)
5. **Blocks are free-form numbers, no existence check.** You may block a number
   that isn't a character yet; the resolve-time filter is where it bites. A
   pre-emptive block must be allowed and must not reveal whether the number is
   real.

### The keeper this session (the point of rule 4)

**The adversarial review found the read-path gap the happy-path tests didn't.**
Every directory *write* guards `number.len() > NUMBER_MAX`, but I'd left the
*read* path (`resolve` / `resolve_public`) uncapped — a multi-megabyte `number`
would reach an indexed lookup unbounded. The tests were green because no test
sent a pathological number; the fan-out reviewer sent one on paper. Two fixes
landed: the cap moved into the shared `resolve` (root-cause: one guard now
protects open_direct, resolve_public, and Sprint 6's future `calls.start`) plus
an early cap in `resolve_public` so its second query short-circuits too. The
second reviewer caught an unbounded `ttl_secs` (i64::MAX overflows
`now() + make_interval` into a 500 instead of a clean `Invalid`) and a vacuous
`.all()`-over-empty-vec test assertion. All three were real, all three fixed,
each now has a test. Lesson: **write-path validation does not cover the read
path**, and a happy-path suite will not surface it — the adversarial pass is the
budgeted verification that does (ADR-1: verification is work, not polish).

### Verification (rule 4)

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean.
- Full suite green across every binary (real Postgres + Redis; MinIO not needed
  — the one media touch, avatar ownership, is a DB count over a directly-seeded
  `live` row). New `tests/directory.rs` (5 tests):
  - `contacts_crud_roundtrip`: create → list → upsert-replaces → owned-vs-foreign
    avatar → idempotent delete.
  - `block_unblock_and_list`: idempotent block, list, unblock.
  - `resolve_unknown_known_and_blocked_indistinguishable`: unknown→false,
    known→true+own-label, **callee-side and caller-side block both → false**
    (the privacy invariant), plus the over-long/empty number cap.
  - `open_direct_blocked_pair_both_directions`: baseline opens; caller-side block
    → `NotFound`; unblock restores; callee-side block → `NotFound`.
  - `listings_create_list_delete_expire`: negative + i64::MAX ttl rejected;
    cursor paging (no dup/skip across the boundary); delete-not-yours →
    `NotFound`; owner delete; expiry hidden at read time then janitor deletes
    exactly one.
- Two adversarial reviewers (privacy/security, correctness/SQL) confirmed the
  block≡unknown invariant, both-direction block correctness, world isolation, and
  no character-id leak on any directory path.

### Exit criteria status (Sprint 5 — now fully closed)

| Criterion | Status |
|---|---|
| Photo round-trips dev-stack | **PASS** (part A). |
| Verification sweep catches a cap bypass | **PASS** (part A). |
| All media/directory commands in coverage match-test; HTTP routes in route-coverage test | **PASS** — directory commands now in the match-test; directory adds no HTTP routes, so the route side is trivially satisfied. |

### Reflection

- **The table seam paid a third time.** Media↔directory (Sprint 5) and the two
  Sprint-4 halves before it: splitting on shared-nothing boundaries keeps each
  half a complete, reviewable, green slice. Cheap dividend, taken again.
- **Adversarial review earned its keep.** Three real fixes from two agents on a
  ~400-line primitive, none of which the happy-path suite would have caught. On a
  stability-first project this is exactly the budgeted verification ADR-1 buys —
  not a nicety.
- **Root-cause over symptom held.** The number cap went into the shared `resolve`
  (one guard, every caller including Sprint 6's) rather than patched per-handler.
- **Still not committed.** Six sprints of work remain untracked. The first push is
  still the single highest-leverage undone thing — it now arms the drift gate with
  three more binding files and unblocks Sprint 4's three-night perf smoke.

### Post-review addendum (same night)

A second review pass over part B's deviations ruled all five justified and
found two coverage conventions that had silently lapsed; both closed now:

1. **Cross-world RLS canary restored** (`directory::cross_world_rls_isolation`):
   raw unfiltered `SELECT count(*)` per table under the *other* world's tx →
   zero, and under the owning world → one (no vacuous pass). Covers `contacts`,
   `blocks`, `listings` **and `media`** — part A had dropped the Sprint-1
   per-table canary convention and part B initially continued the lapse. This
   matters most for `blocks`: resolve's subqueries carry no `world_id`
   predicate, so RLS is the only thing scoping them — precisely the "RLS
   forgotten on a new table" risk-table row.
2. **Golden wire-shape tests back-filled** (`contracts/tests/wire.rs`, 13 → 20
   tests): the Sprint-0 convention ("each Cmd variant, golden JSON strings")
   had stopped at Sprint 2 — channels, notify, media all landed without
   goldens. All 26 missing commands now pinned. The bindings drift gate covers
   naming, not payload shape; the goldens are what would catch a serde attr
   regression before a client does.
3. **OPN-CORE.md §10.7 amended** (design-doc-first rule, retroactively):
   `owner_character` added to the listings tuple with a dated note, and the
   block-scope consequence made explicit — a block gates *new* reach only; an
   already-open thread keeps flowing, deliberately, because killing the thread
   the moment the block lands would reveal the block (same privacy rule as
   resolve). The doc now says "do not fix this by gating `channels.send`" so
   nobody helpfully breaks it later.

4. **CI got MinIO** — first `cargo test --workspace` run on CI failed the three
   live-MinIO media tests with `ConnectionRefused :9000`: the workflow only
   started `postgres redis`, and its own comment ("MinIO joins when media tests
   land") was the forgotten follow-up from part A. Fixed: `up -d --wait` now
   includes `minio`, and the one-shot `createbucket` runs as a separate
   `run --rm` step (it exits by design, so it can't ride `--wait`; `mc mb -p`
   makes it idempotent — verified locally, exit 0 on an existing bucket).

Lesson: conventions enforced by discipline (per-sprint canary, per-command
golden) drift exactly the way the roadmap's compiler-enforced ones don't —
both lapses started the first sprint after their pattern was established and
went unnoticed for two more. Sprint 9's generated RLS tests will make the
canary compiler-shaped; the goldens have no such generator, so they stay a
review-checklist item.

### Next session

1. **The first push** — unchanged, still the critical path (drift gate +
   Sprint 4's three-night smoke).
2. **Sprint 6 — calls + tenant link.** The FSM-as-data, signaling relay,
   `/link` gateway, re-sync. Depends on Sprint 5 (block check at `calls.start`
   reuses the now-caller-aware `resolve`; notify ring class from Sprint 3) —
   all in place.
3. **Media loose ends (small, non-blocking):** `media.favourite` + object
   tagging; the `expired` tombstone state.
4. Still open from before: online-member badging, Bearer case-sensitivity,
   `identity.me` own-last-seen.

---

## 2026-07-18 (night, latest) — Sprint 5 **part A** (media: presigned uploads + janitor verify + gallery + attachment un-gate): built, verified live against MinIO; directory (part B) deferred

Same pacing move as Sprint 4: Sprint 5 has two disjoint halves — **(A)** media
(schema, presigned POST uploads, commit, janitor pending-reap + live
verification, gallery, and un-gating the channels attachment check) and **(B)**
directory (contacts, blocks, `resolve` upgrade, listings, block enforcement at
`open_direct`). They share no tables and no code, so A shipped and stopped here.
No roadmap amendment — items 7–8 stay in Sprint 5.

### What exists now

- **Migration `0007_media.sql`** — one `media` table with a `pending → live`
  lifecycle, `verified_at` (NULLS-FIRST verify cursor), `has_thumb`, and the
  0001 NULLIF world-isolation convention. Three partial indexes: owner-live
  gallery `(owner_character, created_at DESC, id DESC)`, verify cursor
  `(world_id, verified_at NULLS FIRST, id)`, pending-reap `(created_at)`.
- **`infra/s3.rs`** — a hand-rolled minimal S3 client, no SDK. One AWS4
  signing-key HMAC chain feeds two surfaces: **presigned POST policies** (the
  `content-length-range` is what makes the size cap MinIO-enforced, not
  advisory) and **query-signed presigned URLs** (GET for the gallery, HEAD/
  DELETE for the janitor, executed with `reqwest`). Path-style throughout →
  MinIO-native. Unit-tested against an independently-computed signing-key vector
  *and* proven by real MinIO accepting every upload/GET/HEAD.
- **`primitives/media.rs`** — `request_upload` (kind/mime/cap validation →
  pending row → POST policies, original + thumb for photo/video), `commit`
  (owner-scoped `pending → live`), `all_owned_live` (the attachment gate),
  `list` (own live gallery, cursor idiom, fresh presigned GETs per row), and the
  two janitor helpers `reap_pending` + `verify_live` (concurrent HEADs via
  `buffer_unordered(16)`, revert cap-bypassers/missing to pending).
- **`media.request_upload` / `media.commit`** wired through dispatch (+ rate
  classes: request_upload → `Expensive`, commit → `Social`); **`GET /v1/media`**
  gallery route; three new contracts types (`MediaKind`, `UploadTicket` /
  `UploadTarget`, `MediaItem`) exported to bindings.
- **Channels attachment check un-gated** (Sprint 3 item 3 → Sprint 5 item 6):
  `validate_body` no longer hard-rejects `media_ids`; `send` now calls
  `media::all_owned_live` and forbids any attachment that isn't a live row owned
  by the sender. The stale `media_gated_off_until_sprint5` unit test became
  `media_passes_shape_validation`.
- **Two new janitor tasks** (`media_pending_reap`, `media_verify`) on the 30 s
  tick, each walking worlds and doing S3 calls after their SQL.

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **No S3 SDK — hand-rolled SigV4.** The roadmap said "use aws-sdk-s3, don't
   hand-roll SigV4," believing the SDK does presigned POST. It does not (that's
   boto3/JS, not the Rust SDK) — so the POST *policy* signature is hand-rolled
   no matter what. Given that, and a large SDK tree on a RAM-constrained host,
   the lazy path was one HMAC chain feeding both POST policies and presigned
   GET/HEAD/DELETE, with `reqwest` (rustls already in-tree) as the only new
   runtime dep. The "don't hand-roll" caution is answered by a pinned signing-key
   vector plus live MinIO round-trips, not by a heavier dependency.
2. **`aws-sdk-s3` size beat correctness anyway.** Even for the janitor's
   HEAD/DELETE, presigned-URL-then-`reqwest` reuses the same signer — no request
   header-signing code, no SigV4 canonicalization beyond one query string.
3. **Foreign commit → `forbidden`, unknown → `not_found`.** The happy `UPDATE …
   RETURNING` collapses both to zero rows; only the failure path runs a second
   RLS-scoped existence probe to split them. Matches the roadmap test wording;
   media ids are unguessable so leaking "exists" costs nothing.
4. **Thumb pinned to `image/jpeg`, small cap.** A POST policy must condition on
   `Content-Type` (S3 rejects un-conditioned form fields), so the thumb target
   fixes one shape rather than mirror the original kind. `ponytail:` marked.
5. **Verify releases the advisory lock before the HEADs.** Holding a DB
   transaction across ≤500 network HEADs would tie up a pool connection; instead
   read-batch commits, HEADs run lock-free, a second tx applies results. Two
   concurrent janitors at worst re-HEAD a batch — wasteful, never wrong (updates
   idempotent, rule 7 holds).
6. **Deferred, explicitly, to part B or later:** `media.favourite` (object
   tagging is the one awkward S3 op — PUT `?tagging` with body hash); the
   `expired` tombstone state (a lifecycle-deleted object currently reverts to
   pending → reap deletes the row, so galleries lose it rather than tombstone
   it). Neither blocks any Sprint 5 exit criterion.

### The keeper this session (the point of rule 4)

**The SigV4 unit test failed while every live MinIO test passed** — and the
*test* was wrong, not the code. I'd hardcoded `c4afb1cc…` as the expected
signing key from memory; the real canonical key for that AWS example is
`2c94c0cf…` (`c4afb1cc…` is a *signature* from a different example, not a
signing key). MinIO accepting real uploads/GETs/HEADs was the independent proof
the derivation was right; an out-of-band `hmac`+`hashlib` chain confirmed the
value before I changed the constant. Lesson: a self-check pinned to a
half-remembered constant can fail *against correct code* — cross-check the
fixture against something that isn't your memory. The live integration is what
told me which of the two was lying.

### Verification (rule 4)

- `cargo build` + `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt
  --check` clean.
- Full suite green across every binary. New `tests/media.rs` (4 tests, live
  MinIO from the dev stack) proves each exit criterion:
  - `request_upload_commit_roundtrip`: request → real multipart POST to MinIO →
    commit → appears in `GET /v1/media` → **bytes fetched back through the
    presigned GET match** → attaches to a `channels.send`; an unowned id on the
    same send is `forbidden`.
  - `caps_enforced`: over-cap/bad-mime rejected by Core pre-S3; an over-range
    POST **rejected by MinIO itself** (4xx from the policy, not Core).
  - `commit_foreign_forbidden`: other-owner commit → `forbidden`, unknown →
    `not_found`.
  - `verify_reverts_cap_bypass`: a bigger object uploaded through a forged laxer
    policy is **caught by the verify HEAD and reverted to pending** — the cap
    bypass provably fails.
- SigV4 signing-key unit test pinned to the verified vector.

### Exit criteria status (Sprint 5)

| Criterion | Status |
|---|---|
| Photo round-trips dev-stack (request → upload → commit → list → attach → presigned GET), scripted in-repo | **PASS** — `request_upload_commit_roundtrip`. |
| Verification sweep provably catches a cap bypass | **PASS** — `verify_reverts_cap_bypass`. |
| All media/directory commands in coverage match-test; HTTP routes in route-coverage test | **PARTIAL** — media commands + `/v1/media` covered; directory commands land in part B. |

### Reflection

- **Splitting on the table seam worked a second time.** Media and directory
  share nothing, so part A is a complete, reviewed, green vertical slice and
  part B starts from a clean base. Same dividend Sprint 4 paid.
- **The roadmap's library call was wrong, and finding that out early was
  cheap.** Two minutes checking "does the Rust SDK do presigned POST" (it
  doesn't) saved a large dependency and a wrong mental model. The roadmap is a
  plan, not a spec — deviating with a one-line reason in the log is the system
  working.
- **Groundwork from Sprint 0 paid off.** MinIO + bucket were already in compose,
  S3 config already in `Config`/`.env.example` — so this session added no infra
  scaffolding, only the client and the primitive.
- **Not committed.** The pile is now five-and-a-half sprints tall and still
  untracked. Every session this stays the one highest-leverage undone thing: it
  arms the drift gate (now with four new binding files) and unblocks Sprint 4's
  three-night smoke.

### Next session

1. **The first push** — unchanged and still on the critical path: arms the
   contracts drift gate (Cmd/ClientFrame changed, four media `.ts` files added)
   and Sprint 4's three-night perf smoke.
2. **Sprint 5 part B — directory.** contacts CRUD, blocks, the `resolve`
   upgrade (blocked pair → `None`, indistinguishable from unknown), listings
   with janitor expiry, and block enforcement at `channels.open_direct` (both
   directions → `not_found`-equivalent). Then the coverage match-test closes on
   the directory commands.
3. **Media loose ends (small, non-blocking):** `media.favourite` + object
   tagging; the `expired` tombstone state so galleries render tombstones instead
   of dropping lifecycle-expired rows.
4. Still open from before: online-member badging, Bearer case-sensitivity,
   `identity.me` own-last-seen.

---

## 2026-07-18 (night, later) — CI armed by the first push, and caught a migration role-race on run one

The first push finally happened (burning down Sprint 0's CI/drift-gate criterion,
open since the beginning). CI's `test` job went red immediately — not on any new
code, but on a **pre-existing flaky race** in migration `0001` that had never
surfaced across five sessions of local runs, because the local cluster already
had the `opn_app` role from a prior boot and the race only fires when it is
absent.

### The bug

`0001` creates the runtime `opn_app` role, guarded (Sprint 0 decision 3) with
`IF NOT EXISTS (…pg_roles…) THEN CREATE ROLE … EXCEPTION WHEN duplicate_object`.
`#[sqlx::test]` makes a fresh database per test and re-runs every migration; roles
are **cluster-wide**, so on a fresh cluster N tests concurrently pass the
`NOT EXISTS` check and all issue `CREATE ROLE opn_app`. The guard anticipated the
race but caught the wrong error: a *serialized* loser gets `duplicate_object`
(42710), but two *truly concurrent* creates collide at the `pg_authid` unique
index and raise `unique_violation` (**23505**) — which the handler did not catch.
`create_cap_rejected` drew the short straw and failed in migration setup.

### The fix

`EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL` — swallow both
forms of "someone else created it". One line, at the single guard every test's
migration run routes through.

### Verified (the reproduction is the point)

`docker compose down -v` to wipe the volume → fresh cluster with **`opn_app`
absent** (confirmed `SELECT count(*) … = 0`), which is exactly the CI condition
local runs never reproduced. Then `cargo test --workspace`: **all green, zero
failures**, `channels.rs` back to `8 passed`. The wipe re-arms the race; the fix
holds through it.

### Reflection

- **The push paid for itself on the first run.** The whole "why push" question
  from earlier this session got its empirical answer: CI is a *different
  environment* (fresh cluster, real concurrency) and it caught a latent race a
  warm local cluster structurally cannot. This is "real-runtime-catches-desk-
  checks" one more layer out — past Postgres semantics, the async scheduler, and
  the gateway cap, now the **cluster-global role namespace under test
  concurrency**.
- **The lesson for local verification:** a green local `cargo test` on a
  long-lived dev cluster is not the same test CI runs. Anything touching
  cluster-wide objects (roles, tablespaces) needs a `down -v` wipe to exercise
  the cold-start path. Worth doing before the *next* migration that creates a
  cluster-global object.

---

## 2026-07-18 (night) — Sprint 4 **part B** (`opn-loadgen` v0 + nightly perf-smoke machinery): built, verified live; three-night criterion still OPEN

Closes the loadgen half deferred from part A. The load generator exists, runs
end to end against a live stack, and the nightly perf smoke is wired — but its
exit criterion ("green three consecutive nights") cannot *close* until there is
a remote to run the schedule on. All the machinery is in place; the clock is the
only thing left.

### What exists now

- **`crates/loadgen/` — the whole crate** (was a one-line placeholder):
  - `http.rs` — a ~40-line plaintext HTTP mint client (`POST … Connection: close`,
    read-to-EOF). Seeds the population over the **real** mint API, treating Core
    as a black box, exactly as the roadmap wants ("`--seed` mode that calls the
    mint API").
  - `driver.rs` — one WS connection's lifecycle + measurement. Connections run in
    **pairs**: the `Left` half `open_direct`s the `Right` half's number, hands the
    `channel_id` over a `oneshot`, both `sub ch:<id>`, then both send. Every send
    embeds a monotonic microsecond stamp in `body.meta.t`; the **peer** (same
    process, same `Instant` epoch) reads it back off the `channels.message` push
    to compute clock-safe cross-connection delivery latency. Ack RTT is a
    `pending: HashMap<frame_id, Instant>` matched on `reply_to`.
  - `main.rs` — scenario load (JSON), seed, aligned launch, merge, report, exit.
  - `scenarios/smoke.json` — the committed nightly scenario (300 conns, 30 msg/s,
    300 s, gates ack p99 < 25 ms + zero durable closes). `api_key: ""`, injected
    at runtime via `OPN_LOADGEN_API_KEY`.
- **`.github/workflows/perf-smoke.yml`** — `schedule` (nightly `0 3 * * *`) +
  `workflow_dispatch` only, never push/PR. Release build, backgrounded server +
  healthz poll, tenant mint → key capture via `$GITHUB_ENV`, self-asserting
  loadgen run, summary artifact, teardown. (Cross-cutting rule 5.)
- **README `## Load testing`** and the whole thing is clippy `-D warnings` /
  `cargo fmt` clean; 4 new loadgen unit tests (percentiles, host parse, HTTP
  header/body split) — all green.
- **Zero new crates in `Cargo.lock`.** loadgen's deps (`tokio-tungstenite`,
  `futures-util`, `anyhow`) were already compiled for core's tests/deps. The tool
  cost the lockfile nothing.

### Decisions closed during implementation (roadmap deviations, all ponytail)

The roadmap names three specific tools for loadgen; all three were shed, each for
the codebase's established "one less dependency" reason (cf. hand-rolled cursor,
tenant cache, gif allowlist):

1. **hdrhistogram → exact sorted-`Vec` percentiles.** The v0 smoke is ~9 k
   samples where an exact nearest-rank percentile beats a bucketed estimate and
   needs no dep. Marked with a `ponytail:` note: the Vec is fine to ~1 M samples;
   **Sprint 10's 24 h soak** will record hundreds of millions and wants
   hdrhistogram or reservoir sampling *then*, not now.
2. **TOML → JSON scenario.** `serde_json` is already a workspace dep; the config
   is six fields; a committed named scenario file (the actual point) works
   identically. No `toml` crate.
3. **reqwest → hand-rolled plaintext HTTP.** loadgen only ever mints against a
   local/compose Core over plain HTTP; one request shape with `Connection: close`
   is ~40 lines, versus reqwest's ~50-crate tree. Noted: reach for reqwest if a
   TLS endpoint or connection reuse ever appears.
4. **Aligned start via a warmup instant, not `tokio::sync::Barrier`.** All
   connections compute one shared `start_at = epoch + warmup` and `interval_at`
   their first send to it. A real `Barrier` of size N deadlocks the whole run if
   one connection fails setup; the instant does not.
5. **Delivery latency counts peer messages only** (`sender != own char`). The
   sender's own fan-out copy would measure the loopback path, not cross-connection
   delivery — a truer-but-faster number that would flatter the p99.

### The two findings the live run caught (this session's keepers)

Both surfaced on the **first** real end-to-end run — neither is visible to a
desk-check or a compile:

1. **The gateway's own per-IP pre-auth cap breaks single-IP load tests.**
   `OPN_PREAUTH_PER_IP_MAX` defaults to **5** (§4.1 admission control). A loadgen
   runs every connection from `127.0.0.1`, so the first run reported **7 of 10
   connections 429'd** before the WS upgrade. The 300-conn nightly smoke would
   have reported ~295 errors and exited 2 on its *first scheduled night* — a
   green-looking machine failing for a reason unrelated to performance. Fix: the
   load-test deployment must raise the cap above the connection count. Added
   `OPN_PREAUTH_PER_IP_MAX: '400'` to `perf-smoke.yml`'s env and a note to the
   README. This is "real-runtime-catches-desk-checks" reaching a *new* layer —
   past Postgres (Sprints 0/1) and the async scheduler (part A), now the
   gateway's admission control.
2. **Aggregate send rate is bounded by `connections × Msg-budget`.** The `Msg`
   rate class is 1.0/s sustained (5 burst). A first quick scenario (10 conns,
   20 msg/s → 2/s per conn) drew 6 `rate_limited` acks. The committed smoke
   (30 msg/s over 300 conns = 0.1/s each) is comfortably under, but the ceiling
   is real: a valid scenario needs `total_msgs_per_sec ≤ connections`. loadgen
   handles a `rate_limited` ack gracefully (counts it, excludes it from ack-RTT)
   rather than treating it as an error.

Neither is a loadgen bug — the tool reported both accurately, which is the tool
working. But #1 would have made the nightly smoke red for the wrong reason, so
catching it now (before any push) is the session's actual save.

### Verification (rule 4)

Live e2e against the compose stack + real server: **20 conns, 10 msg/s, 6 s →
PASS**. 0 errors, 0 `rate_limited`, 0 durable/other closes; 80 sends / 160 recvs
(two subscribers per channel) / 80 peer-deliveries; ack p99 **21.8 ms**, delivery
p99 **21.8 ms** — in a **debug** build, so release (the smoke's mandate) will be
far under the 25 ms gate. Seeding, pairing, cross-connection delivery
measurement, ack correlation, JSON summary, human table, and the 0/1/2 exit codes
all exercised.

### Exit criteria status (Sprint 4, updated)

| Criterion | Status |
|---|---|
| Every `channels.*` command in the coverage match-test | **PASS** (part A). |
| Nightly perf smoke live and green three consecutive nights | **OPEN — machinery complete.** Workflow, scenario, and self-asserting binary all exist and pass a live run; only a remote + three scheduled nights remain. Blocked on the same first-push that has been open since Sprint 0. |
| Messages surface demo-able end to end vs the shell dev build | **N/A** — coordination point with opn-ui, not a blocker. |

### Reflection

- **The subagent recipe held, with one sequencing lesson.** Main thread wrote the
  whole coupled crate (the concurrency + measurement core has no independent test
  leg to split off — the loadgen *is* the test tool); one agent wrote the CI
  workflow + README + the exit-code-preservation shell in parallel, off a fixed
  CLI contract. It even improved on my instruction — I'd suggested `> file;
  code=$?`, which `set -e` aborts before the capture; the agent used `|| code=$?`,
  which survives. The independence earned its keep again.
- **But the independent leg can't know what the live run hasn't taught yet.** The
  agent copied the CI env verbatim from the `test` job — correct at the time — and
  I had to patch in `OPN_PREAUTH_PER_IP_MAX` *after* the live run revealed the
  429. The infra author finalized before the empirical finding existed. Lesson for
  next time: run the smoke once locally to discovery-completion *before* handing
  the CI env to an agent, or expect to patch its env after.
- **Splitting Sprint 4 was vindicated twice over.** Part A shipped a reviewed
  feature surface; part B got a loadgen designed without time pressure, and the
  gateway-cap finding had room to surface. Bundling would have buried both.
- **Not committed** — the pile is now four-and-a-half sprints tall and still
  untracked. The first push is no longer just hygiene: it is the literal
  precondition for closing Sprint 4's last exit criterion (the nightly smoke can't
  run without a remote) *and* Sprint 0's CI/drift-gate criterion. That is the one
  thing worth doing before more code.

### Next session

1. **The first push** — it now unblocks two sprints at once (Sprint 0's CI/drift
   gate, Sprint 4's three-night smoke) and arms the contracts drift gate for
   everything since. This has been "the operator's call" for five sessions; it is
   now on the critical path.
2. **Sprint 5 — Media + directory.** Un-gates the `channels` attachment check
   (Sprint 3 decision 6) into the real owned+live count; presigned MinIO uploads,
   janitor verification sweep, contacts/blocks/listings, block enforcement at
   `open_direct`. MinIO joins the compose `--wait` set in CI here.
3. Still open, minor, none blocking: online-member badging (Sprint 3 dec. 9),
   Bearer-scheme case-sensitivity, `identity.me` own-last-seen (part A dec. 6).

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
