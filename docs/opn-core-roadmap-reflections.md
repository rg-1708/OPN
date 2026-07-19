# OPN-CORE Roadmap — Implementation Reflections

Running log, one section per work session. Newest first. Companion to
[opn-core-roadmap.md](opn-core-roadmap.md); design-level amendments still go
to OPN-CORE.md as CDRs — this file records *how the build actually went*.

---

## 2026-07-20 (latest) — Sprint 9 **part B2b/3** (chaos, link-drop slice: a fake `/link` consumer as loadgen `--link-drop` + `http::get` + `chaos/link-drop.sh` on the shared `chaos/lib.sh` + `just chaos`/`chaos.yml` running **all four** drills): built, verified live (three consecutive green runs, one keeper caught on run one). **Sprint 9 is now CLOSED.**

B2b/2 named B2b/3 as "the last chaos drill — `link-drop`", needing "its own new tool (a fake `/link` WS consumer)". That is exactly the seam: unlike pg-restart (rode B2a's verifier verbatim) and redis-restart (a second Core + `--xinstance`), link-drop's fault is **resource-side** — the tenant `/link` consumer (a FiveM server, out-of-repo) crashes and reconnects. There is no infra fault for bash to inject; the checker drops *its own* `/link` socket and reconnects it, Core untouched. So B2b/3 is the fourth clean cut on the same new-machinery boundary the project has taken since Sprint 4: a new tool (the `/link` consumer), one thin drill script, no change to the finished harness beyond one `http::get`. It closes roadmap item 3's `link-drop` bullet — "reconnect → re-sync returns the active call and a subsequent accept emits targets" — verbatim, and with it Sprint 9.

### What exists now

- **loadgen `--link-drop <http> <ws> [drop_gap_secs]`** (`crates/loadgen/src/linkdrop.rs`, new; wired in `main.rs`) — the fake tenant resource. One long-lived process (the `--xinstance` shape) that: mints **four** characters, connects two `/ws` sessions (caller1/callee1) + a `/link` consumer, drives call #1 to `active`, and asserts the link receives `calls.voice set_targets` (**PRE**). Then it **drops the `/link` socket** (`drop(link)` — a hard client disconnect, the resource crash), waits `drop_gap_secs` (default 3), **reconnects** the link + redoes the hello, **re-syncs** via `GET /v1/tenants/self/calls/active` and asserts the still-active call #1 is returned, then starts+accepts **call #2** and asserts the *reconnected* link receives its `set_targets` (**POST**). Exit 0 = both invariants held; 1 = a `set_targets` was lost or the re-sync missed the call; 2 = setup error — the same convention as `--verify-resume`/`--xinstance`. Reuses `driver::{send, await_ack}` and `http::mint`; the `/link` hello/ack dance, the API-key-header WS connect, and the `set_targets`/active-call matchers are the only new code.
- **`http::get`** (`crates/loadgen/src/http.rs`, new) — a one-shot `GET {path}` with the tenant API-key `Authorization` header, hand-rolled exactly like `http::mint` (HTTP/1.1 + `Connection: close`, no `reqwest`). The re-sync read's only new dependency-free primitive.
- **`chaos/link-drop.sh`** (new) — the thinnest drill yet: `stack_up` → `build_release` → one Core → `mint_tenant` → `--link-drop`. No fault injection in bash (the checker injects its own), so `set -e` propagates the checker's exit straight out; a trailing `log PASS` only prints on success. Single Core, no two-instance topology, no `redis-cli`.
- **`justfile` `chaos`** and **`.github/workflows/chaos.yml`** now run **all four** drills (kill9 → pg-restart → redis-restart → link-drop).

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **The fault is in-process, not a docker fault.** kill9/pg/redis all inject an *infra* fault (crash Core, stop Postgres, SIGKILL Redis) via `docker compose`. link-drop's fault is a *client* disconnect — the resource is the thing that fails — so the checker drops its own `/link` socket. This makes `link-drop.sh` the simplest drill (no fault helper on `lib.sh` at all) and is the faithful model: Core is never the failure, the resource is.
2. **Fresh parties for the post-reconnect call — the busy trap.** Call #1's caller and callee stay `joined` in the un-ended session across the drop, so `calls.start` would reject them `busy` (any participant in a non-ended session). The "subsequent accept" therefore needs two *new* numbers — call #2 between caller2/callee2. Discovered from the calls busy-check + FSM before the first live run, not after a failure.
3. **"A subsequent accept emits targets" is call #2's accept on the reconnected link, not a same-party re-accept.** The FSM (`fsm.rs`) makes a re-accept by an already-`Joined` participant a `conflict` (`actor != Ringing → Err`), so there is no idempotent "re-emit on re-accept." The roadmap's re-sync recovery is the HTTP `GET /calls/active` (call #1 still active); the "targets re-emit" is a *fresh* call's accept reaching the reconnected link — the resubscribe analog of redis-restart's POST hop. Both facts the bullet names, via the only legal paths.
4. **Derive `/link` from the `/ws` base, no third CLI arg.** The harness passes the `/ws` gateway URL; `link_url` swaps the `/ws` suffix for `/link` on the same Core. One URL in, one unit test on the swap.
5. **`http::get` hand-rolled beside `http::mint`, not a `reqwest` pull-in.** One more one-shot request shape; the module header's "reach for `reqwest` only if TLS/keep-alive is ever needed" still holds.

### The keeper this session: a real harness wiring bug — the consumer pointed at `/ws`, not `/link` — caught on run one

Unlike B1/B2a/B2b/1 (no keeper — a net over an existing guarantee) and like B2b/2 (whose keeper was a *vacuity* trap), B2b/3 **had a keeper, and it was a genuine wiring defect in the harness**: the first live run closed immediately with `link closed before hello ack: CloseFrame { code: 4400, reason: "expected auth frame" }`. Root cause: the checker connected the `/link` consumer to the same `/ws` URL it used for caller/callee — and `/ws` closes `4400` on any non-`auth` first frame, while `/link` is a *different endpoint* that expects a raw `LinkHello`. A compiled-and-unit-tested slice would have shipped this: every unit test passed, `fmt`/`clippy` were clean, and the bug lived entirely in *which endpoint the WS opened* — invisible to anything short of a real `/link` gateway on the other end. This is the exact adversarial value the §15 budget buys for a chaos slice: **running the drill against the real server is the test**; a drill that "passes its unit tests" but points at the wrong endpoint exercises nothing. Fixed at the root (derive `/link` from `/ws`, `link_url` + a unit test pinning the swap), not by loosening a timeout. The permanent guards the slice leaves are the **5 loadgen unit tests** — `set_targets` matches only its own call id, `clear` is *not* `set_targets`, `active_has_call` requires the `active` state (not `ringing`), `link_url` swaps `/ws`→`/link`, host-strip — each pinning a matcher whose silent drift would make the drill pass vacuously.

### Verification (rule 4)

- `cargo fmt -p opn-loadgen --check` clean; `cargo clippy -p opn-loadgen --all-targets -- -D warnings` clean (the new module is `let else`/`if let`/`matches!`/`bail!`, no unwrap; the `core` crate's `unwrap_used` deny is untouched — no core change).
- **16 loadgen unit tests green** (11 prior + 5 new: the four matchers above + the `/link` URL swap).
- **`link-drop.sh` run three consecutive times against the real stack** (docker compose: Postgres 16 + Redis 7 + MinIO, a single release Core, on the dev host i5-14500): each run **`PRE set_targets OK` → `re-sync returned the active call` → `POST set_targets OK` → PASS, exit 0** — the full pipeline (mint four → connect two `/ws` + a `/link` consumer → call #1 accept → link `set_targets` → drop the link → reconnect → HTTP re-sync of the active call → call #2 accept → `set_targets` on the reconnected link) exercised end to end. Run zero (pre-fix) is the keeper's evidence: the `4400 "expected auth frame"` close.
- Drift gate untouched — `--link-drop` only *reads* the `call_id` an ack already carries and the `calls.voice`/`ActiveCall` shapes already on the wire; no wire type changed, no `.d.ts` diff.

### Exit criteria status (Sprint 9 — **all closed**)

| Criterion (roadmap Sprint 9) | Status |
|---|---|
| All four proptest suites green at 1024 cases locally | **CLOSED (B1)**. |
| 24 h fuzz per target, zero crashes | **CLOSED-modulo-burn-in (B1)** — targets build + smoke clean at millions of execs; the one-time 24 h burn-in is an operator out-of-band run, not a code deliverable. |
| `just chaos` green three consecutive runs; weekly CI | **CLOSED** — all four drills (`kill9-mid-send`, `pg-restart`, `redis-restart`, `link-drop`) green three consecutive runs, all in `just chaos` + weekly `chaos.yml`. |
| Generated RLS test covers every world_id table | **CLOSED (part A)**. |
| Dependency audit gate (`cargo deny`) | **CLOSED (part A, pending first-CI tune)**. |

### Reflection

- **The new-machinery seam held for the fourth and last time.** link-drop needed exactly one new tool (a `/link` consumer) and nothing on the finished harness but a one-shot `http::get` — the same clean cut the project has taken at every boundary since Sprint 4. Sprint 9's five slices (A, B1, B2a, B2b/1, B2b/2, B2b/3) each split on the *delivery-mechanism* or *new-machinery* seam, never a primitive seam, and each was self-contained and locally verifiable. That is the shape a hardening sprint wants.
- **A chaos slice's failure mode is a false green — and this time it was a false *nothing*.** B2b/2's keeper was a drill that passed without injecting its fault; B2b/3's was a drill that *connected to the wrong endpoint* and would have "passed its unit tests" while testing an empty room. Both prove the same lesson: for a fault harness, green unit tests are necessary and worthless — only the real end-to-end run tells you the fault occurred and the invariant held. Running it is the test. ADR-1's budget is what buys the run.
- **Ponytail held on the tool and the drill.** An in-process fault over a docker one (the fault *is* client-side); fresh parties over fighting the busy-check; the HTTP re-sync + a fresh call over inventing a re-emit path the FSM doesn't have; `link_url` derivation over a third CLI arg; `http::get` beside `http::mint` over `reqwest`. Smallest diff that actually injects the fault and enforces the property — the thinnest of the four drills, fittingly last.

### Not committed / next session

- **Sprint 9 part B2b/3 is complete and green but untracked** on top of committed 0–8 + part A + B1 + B2a + B2b/1 + B2b/2. First commit lands the loadgen `--link-drop` checker (`linkdrop.rs` + `main.rs` wiring + 5 unit tests), `http::get` (`http.rs`), `chaos/link-drop.sh`, and the `just chaos` + `chaos.yml` wiring — the operator's call, as every sprint. Drift gate untouched. **With this slice, Sprint 9 is closed**; all of parts A + B1 + B2a + B2b/1 + B2b/2 + B2b/3 remain a single untracked stack on committed 0–8 for the operator to land.
- **Next: Sprint 10 — Performance & soak.** The scenario battery (`design`, `soak10x`, `reconnect-storm`, `hot-channel`, `call-churn`), production-shaped core-pinned environment, profiling only against flamegraph/`pg_stat` evidence, the six perf targets committed to `perf/results/`, the 24 h 10× soak, and the tightened nightly smoke. It builds directly on the loadgen the chaos slices extended — the `--xinstance`/`--link-drop`/ack-journal work all live in the same `crates/loadgen`, so Sprint 10's new scenarios are more of the same crate, now under a core-pinned harness.
- Minor/non-blocking, carried from prior sessions: the `cargo deny` license allow-list still wants its first-CI pass (part A); the 24 h fuzz burn-in remains an operator out-of-band run; the two-instance `lib.sh` topology (B2b/2) stays redis-restart-specific but reusable.

---

## 2026-07-19 — Sprint 9 **part B2b/2** (chaos, redis-restart slice: a second Core instance + the loadgen `--xinstance` cross-instance delivery checker + `redis_restart` (SIGKILL) on the shared `chaos/lib.sh` + `redis-restart.sh` + `just chaos`/`chaos.yml` running all three drills): built, verified live (three consecutive non-vacuous green runs + a negative control); the last drill (`link-drop`) deferred

B2b/1 (pg-restart) rode B2a's finished machinery **verbatim** — the ack journal + `--verify-resume` verifier — because its core invariant (zero acked-but-lost) is identical to kill9's. redis-restart is the first drill that **cannot**: its invariants live in cross-*replica* fan-out and the presence keyspace, neither of which the single-instance harness or the ack journal touch. So B2b splits again on the **new-machinery seam**: **(B2b/2)** the redis-restart drill — stands up a *second Core instance* sharing one Redis (two-instance pub/sub) and a *new loadgen mode* (`--xinstance`) that proves the A→B hop, both genuinely new orchestration; **(B2b/3)** `link-drop`, the last drill, which needs its own new tool (a fake `/link` WS consumer driving a call). B2b/2 is a self-contained, fully-locally-verifiable slice (docker + two release Cores + a Redis crash all ran on the dev host). No roadmap amendment — this is roadmap item 3's `redis-restart` bullet ("presence keys rebuild within one heartbeat cycle and pub/sub resubscribes (two-instance mode)"), verbatim.

### What exists now

- **loadgen `--xinstance <http> <ws_a> <ws_b> [settle_secs]`** (`crates/loadgen/src/xinstance.rs`, new; wired in `main.rs`) — the cross-instance delivery checker. Mints two members of one DM channel; holds a **sender on Core A** and a **subscriber on Core B** open for the whole window. It proves the A→B hop **once before** the drill restarts Redis and **once after**: each proof sends a nonce-tagged `channels.send` on A, then asserts the matching `channels.message` arrives on B within 15 s. A has *no local subscriber* for the channel, so the only path to B is `PUBLISH opn:… → B's listener → publish_local` — that is what makes it a true cross-instance test, not a loopback. The post-restart proof is the "**pub/sub resubscribed**" gate. Between the two, it drains both sockets (answering pings, keeping both characters online) so each Core's presence refresher has a live character to rewrite `presence:*` for. Exit 0 = both hops crossed; 1 = a delivery was lost (timeout); 2 = setup error — the same code convention as `--verify-resume`. Reuses `driver::{send, await_ack}` (already `pub(crate)`) and `http::mint`; only the nonce-drain loop and the two-socket hold are new.
- **`chaos/lib.sh` two-instance orchestration** — `OPN_BIND_B`/`OPN_METRICS_BIND_B` (`:8081`/`:9091`), `WS_URL_B`/`HTTP_URL_B`, and `core_start_b`/`core_stop_b`/`core_b_wait_health` for a second release Core sharing the same PG+Redis+MinIO. The second instance only overrides its bind/metrics and inherits `OPN_REPLICAS=2` (the drill exports it), so **both** Cores run the fanout listener. `cleanup` now stops B first.
- **`chaos/lib.sh` `redis_restart` + helpers** — `redis_restart` is a **SIGKILL** (`docker compose kill -s KILL redis`) followed by `up -d --wait`, *not* `docker compose restart` (see keeper). `redis_cli` (`exec -T redis redis-cli …`) and `presence_key_count` (`--scan --pattern 'presence:*' | grep -c .`) give the drill a redis-level view of the key rebuild.
- **`chaos/redis-restart.sh`** (new) — two Cores (A:8080, B:8081), `OPN_REPLICAS=2`, a shorter `OPN_HEARTBEAT_SECS=10` to keep the drill quick. A **two-part invariant**: (1) resubscribe — gated by the `--xinstance` checker's exit (its post-restart hop crossed); (2) presence rebuild — gated **here** via `presence_key_count` (`≥1` within one heartbeat cycle + margin), with a `≥1`-before baseline so the gate can't pass vacuously. Waits for the checker's `PRE delivery OK` on stdout before injecting the fault; logs the immediately-after count as non-vacuity evidence (`0`).
- **`justfile` `chaos`** and **`.github/workflows/chaos.yml`** now run **all three** drills (kill9 → pg-restart → redis-restart).

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **One long-lived `--xinstance` process holding both connections across the window, not two modes + bash signaling.** The single process proves *both* deliveries (its exit is the resubscribe gate) *and* keeps both characters online so `presence:*` persists for the refresher — so bash only has to bounce Redis and watch the key count. The alternative (a one-shot delivery mode run twice + a separate `--hold` mode + stdout coordination) is strictly more moving parts for the same two facts.
2. **SIGKILL, not `docker compose restart` (SIGTERM).** This was the keeper — caught on run one. `restart` sends SIGTERM; redis then snapshots its keyspace to `dump.rdb` and reloads it on start, so `presence:*` *survives* and the rebuild is never exercised (the first run read "immediately after restart: 2" — a green proving nothing). SIGKILL with no save point met and no volume/appendonly in the dev compose brings redis back **empty**, so the refresher genuinely has to rewrite the keys. A crash-restart is also the more realistic and *stronger* fault.
3. **Presence rebuild asserted in bash (redis-cli), cross-instance delivery in Rust (checker exit).** Each sub-invariant goes to the tool that observes it natively: the key count is a Redis fact, best read with `redis-cli`; the delivery is a protocol fact, best proven over the wire. No redis dependency added to loadgen for a `KEYS` call bash already does.
4. **Shorter `OPN_HEARTBEAT_SECS=10` for the drill.** "Within one heartbeat cycle" is faithful at *any* cadence — the refresher's interval *is* the cycle under test — so a 10 s heartbeat shrinks the drill's wall-time (settle 35 s vs 75 s) without weakening the assertion. Overridable.
5. **The checker needs no local subscriber on A.** `gateway::publish` PUBLISHes to Redis unconditionally when `replicas > 1` (not gated on local subs), so a send on A with M2 absent from A's registry still crosses to B. This is exactly why the test is cross-instance and not a same-box loopback — and why the negative control (below) works.

### The keeper this session: a *vacuity* trap in the fault injector, caught by run-one evidence — and a negative control to seal the gate

Unlike B1/B2a/B2b/1 (each added no production keeper — a net over an existing guarantee), B2b/2's keeper was in the **harness itself**, and it was a *false green*: the first drill run passed with `presence keys immediately after restart: 2`. That is the vacuity smell — the keys the drill claims to watch "rebuild" never disappeared, because `docker compose restart`'s graceful SIGTERM let redis persist and reload them. A drill that passes without exercising its invariant is worse than no drill. Root-caused to the signal (SIGTERM → RDB save/reload), not the symptom (fixed by switching to SIGKILL, not by lengthening a wait): the second run read `0 → rebuilt to 2`, non-vacuous. Then, because "the delivery proof passed" is only meaningful if that proof *can* fail, I ran a **negative control** — two Cores with `OPN_REPLICAS=1` (fanout listener never spawned): the checker's PRE delivery timed out and it exited 1 (`GATE IS REAL — checker correctly failed with no fanout`). So both gates are proven non-vacuous: presence by the `0 →` transition, delivery by the REPLICAS=1 refutation. The single permanent guards the slice leaves are the two loadgen unit tests (`guard_flags_a_close` — a close during the hold is a failure, never silently drained; `host_strips_scheme_and_path`) plus the drill's own baseline/immediately-after logging; the adversarial-lens ritual bit on the injector, which is exactly where a chaos slice's bug hides.

### Verification (rule 4)

- `cargo fmt -p opn-loadgen --check` clean; `cargo clippy -p opn-loadgen --all-targets -- -D warnings` clean (the new module is `let else`/`if let`/`bail!`, no unwrap; the `core` crate's `unwrap_used` deny is untouched — no core change).
- **11 loadgen unit tests green** (9 prior + 2 new: `guard_flags_a_close`, `host_strips_scheme_and_path`).
- **`redis-restart.sh` run three consecutive times against the real stack** (docker compose: Postgres 16 + Redis 7 + MinIO, **two** release Cores on :8080/:8081, `OPN_REPLICAS=2`, SIGKILL-restart of Redis, on the dev host i5-14500): each run **`presence keys immediately after restart: 0` → `rebuilt within one heartbeat cycle: 2`** and **`PASS`, exit 0** — the full pipeline (mint two members → sender on A / subscriber on B → PRE hop → hard-kill Redis → presence repopulates → POST hop → clean exit) exercised end to end, non-vacuously.
- **Negative control green**: two Cores with `OPN_REPLICAS=1` → checker exit 1 (PRE delivery cannot cross with no fanout), proving the delivery gate trips.
- Drift gate untouched — `--xinstance` only *reads* the `channels.send` ack and the `channels.message` push it already carries; no wire type changed, no `.d.ts` diff.

### Exit criteria status (Sprint 9 — B2b/2 slice; B2b/3 = `link-drop` still open)

| Criterion (roadmap Sprint 9) | Status |
|---|---|
| All four proptest suites green at 1024 cases locally | **CLOSED (B1)**. |
| 24 h fuzz per target, zero crashes | **PARTIAL → strong (B1)** — targets build + smoke clean; 24 h burn-in is an operator out-of-band run. |
| `just chaos` green three consecutive runs; weekly CI | **PARTIAL → stronger** — `kill9-mid-send` (B2a) + `pg-restart` (B2b/1) + `redis-restart` (this slice) all green three consecutive runs, all in `just chaos` + weekly `chaos.yml`; `link-drop` not built yet, so `just chaos` runs **three of four**. |
| Generated RLS test covers every world_id table | **CLOSED (part A)**. |
| Dependency audit gate (`cargo deny`) | **CLOSED (part A, pending first-CI tune)**. |

### Reflection

- **The new-machinery seam was the right fifth cut.** pg-restart reused B2a's harness whole; redis-restart is the first drill that had to *build* — a two-instance topology and a cross-instance checker — because its invariants are cross-replica, not per-connection. That is precisely the seam B2b split on. `link-drop` needs its own new tool again (a `/link` consumer), so it stays its own slice — the same clean boundary the project has taken since Sprint 4.
- **A chaos slice's success looks like silence — but a chaos slice's *failure mode* is a false green.** kill9/pg-restart confirmed durability + liveness with no keeper; redis-restart's keeper was the drill lying to itself (SIGTERM persistence). The lesson generalizes: for a fault injector, "did it pass" is not enough — "did the fault actually occur" (the `0 →` evidence) and "would the gate fail if the property broke" (the negative control) are the real checks. ADR-1's budget buys that rigor.
- **Ponytail held on the harness.** One long-lived checker over two modes + signaling; redis-cli for the presence fact over a redis crate in loadgen; SIGKILL calibrated to actually destroy the state (the "hardware needs a knob" instinct applied to a fault, not a sensor); the second Core is the same binary with two env overrides, not a new build. Smallest diff that actually enforces the property — and, this time, that actually *injects* it.

### Not committed / next session

- **Sprint 9 part B2b/2 is complete and green but untracked** on top of committed 0–8 + part A + B1 + B2a + B2b/1. First commit lands the loadgen `--xinstance` checker (`xinstance.rs` + `main.rs` wiring + 2 unit tests), the `chaos/lib.sh` two-instance/redis additions, `chaos/redis-restart.sh`, and the `just chaos` + `chaos.yml` wiring — the operator's call, as every sprint. Drift gate untouched.
- **Next: the last chaos drill — `link-drop` (B2b/3).** A fake `/link` WS consumer (a new tool: connect to `GET /link` with the tenant API key, do the hello handshake, receive `calls.voice` events) driving a call to `active`, dropped mid-call → reconnect → `GET /v1/tenants/self/calls/active` re-sync returns the active call and a subsequent `calls.accept` re-emits `set_targets`. It extends `just chaos` + `chaos.yml`. After it, **Sprint 9 is closed** and the project moves to Sprint 10 (performance & soak).
- Minor/non-blocking: the two-instance `lib.sh` topology now exists but is redis-restart-specific — if a future drill wants it, it is already reusable; the `cargo deny` license allow-list still wants its first-CI pass (part A); the 24 h fuzz burn-in remains an operator out-of-band run.

---

## 2026-07-19 — Sprint 9 **part B2b/1** (chaos, pg-restart slice: the loadgen `error_acks` counter + `assert_error_acks` gate + `pg_restart_gap` on the shared `chaos/lib.sh` + `pg-restart.sh` + `just chaos`/`chaos.yml` running both drills): built, verified live (three consecutive green runs); the two remaining drills (`redis-restart`, `link-drop`) deferred

B2a closed the kill9 slice and named B2b as "the three remaining drills, each a thin script on the finished `chaos/lib.sh`", flagging `pg-restart` as **the one that needs a bounded loadgen touch to surface acks-arriving-during-the-gap**. That touch is exactly the seam B2b splits on. Of the three, only `pg-restart` rides B2a's finished machinery — the ack journal + `--verify-resume` verifier — because its core invariant (zero acked-but-lost) is *identical* to kill9's; the fault differs (a graceful DB outage, not a hard Core crash) and it adds one DB-specific assertion (error acks, not silence, during the gap). The other two need genuinely new plumbing: `redis-restart` a **second Core instance** (two-instance pub/sub + presence-key rebuild), `link-drop` a **fake `/link` consumer** driving a full call flow. So B2b splits on the **new-machinery seam**: **(B2b/1)** the pg-restart drill — reuses the verifier verbatim, adds only the pre-flagged `error_acks` touch; **(B2b/2, /3)** the two drills that each stand up new orchestration. B2b/1 is the self-contained, fully-locally-verifiable slice (docker + a release Core + stop/start orchestration all ran on the dev host). No roadmap amendment — this is roadmap item 3's `pg-restart` bullet, verbatim.

### What exists now

- **loadgen `error_acks` counter** (`crates/loadgen/src/driver.rs`, `main.rs`) — `ConnStats` gains `error_acks`, incremented on any tracked-send ack that is `ok = false` **and not** a rate-limit (i.e. `internal`/`not_found`/…). It is the "error acks, not silence" signal: while Core's pool can't reach Postgres it still acks each in-gap send an `internal` (the 3 s `acquire_timeout` fires into an internal ack, §7) rather than hanging the socket. Counted **unconditionally** — a single `u64`, so every perf/soak run gets a free "did any send error" line at zero cost, and it does not gate anything unless a scenario opts in. Surfaced in the JSON summary + human table.
- **`assert_error_acks` scenario gate** (`main.rs`) — a new `#[serde(default)] bool`; when set, loadgen exits 1 if `error_acks == 0` (the gap produced silence, not error acks). Sits beside the existing `assert_ack_p99_ms` / `assert_no_durable_closes` gates — the assertion lives in typed Rust, not bash JSON-grepping.
- **`chaos/lib.sh`** gains `pg_restart_gap` — `stop postgres` → `sleep $gap` (default 6) → `up -d --wait postgres`. **Not `docker restart`**: its ~1 s bounce can be too quick to force a pool acquire timeout; a deterministic 6 s gap > the 3 s `acquire_timeout` *guarantees* in-gap sends get an `internal` ack, so the gate can't flake. Core is never touched — it must ride the outage. The `pgdata` volume persists across stop/start, so committed rows survive.
- **`chaos/pg-restart.sh`** (new) — loadgen at 30 msg/s journaling acks → `sleep 7` past warmup → `pg_restart_gap 6` mid-stream → `core_wait_health` (pool reconnected) → loadgen runs to completion. A **three-part invariant**: (1) no acked message lost — gated by `--verify-resume` (verbatim reuse); (2) error acks not silence — gated by loadgen's `assert_error_acks` exit; (3) recovery — proved by loadgen finishing clean (ok acks resume) + `/healthz` green again. Exit 0/1/2 = held / broke / setup-failure.
- **`crates/loadgen/scenarios/chaos-pg.json`** — 2 conns, 30 msg/s, warmup 3 s, `duration_secs: 25` (long enough to span pre-gap sends + the 6 s outage + post-gap recovery), `assert_error_acks: true`, no perf gates (the drill gates on durability + liveness, not p99).
- **`justfile` `chaos`** and **`.github/workflows/chaos.yml`** now run **both** drills (kill9 then pg-restart). The shared verifier's PASS line was generalized from "survived kill -9" to "survived the fault" — it is now driven by two callers.

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **`--verify-resume` reused verbatim for invariant (1).** pg-restart's "zero acked-but-lost" *is* kill9's whole invariant — persist-then-ack + resume replay, indifferent to whether the fault was a `kill -9` or a graceful PG bounce. The only new code is the DB-specific behavior (error acks in the gap, recovery after). Re-deriving a second verifier would be pure duplication.
2. **`error_acks` counted unconditionally, not gated on `record_acks`.** Unlike the ack journal (an unbounded `Vec`, correctly gated off for the 24 h soak), this is one monotonic counter — free everywhere. Gating it would add a branch to save nothing.
3. **The assertion is a Rust scenario gate, not a bash check on the JSON line.** `assert_error_acks` mirrors the two existing `assert_*` gates exactly — typed, unit-tested, and the drill just consumes loadgen's exit code, the same contract kill9 uses.
4. **Deterministic `stop → sleep → start`, not `docker restart`.** The gate needs the gap to reliably exceed the 3 s pool `acquire_timeout`; a 6 s stop guarantees it, `restart`'s sub-second bounce does not. This is the "hardware needs a calibration knob" instinct applied to a fault injector — the physical timing has to be tuned past the timeout, not assumed.
5. **loadgen runs foreground-to-completion (backgrounded + `wait`ed), and its exit is *checked* — unlike kill9, which ignores it.** In kill9 the verifier is the sole gate because the kill destroys the sockets; here invariants (2) and (3) *are* loadgen's own run (it must survive the outage and keep acking), so its exit is gate (2)+(3) and the verifier is gate (1). `wait "$LG_PID" || LG_CODE=$?` captures the code under `set -e`.

### The keeper this session: none in production code — the expected shape for a chaos slice, plus one empirical confirmation

Like B1 (fuzz) and B2a (kill9), B2b/1 added **no new production logic** — it laid a fault net over an existing guarantee. The one genuinely interesting finding was **empirical and reassuring**: sqlx's `PgPool` reconnects transparently, so **Core rode a 6 s Postgres outage with zero recovery code** — every in-gap send got a clean `internal` ack (never a hang, never a crash, the janitor loop logged and lived per cross-cutting rule 7), and ok acks resumed the instant PG was back, with all 54 pre+post-gap acked messages durable. B2a proved persist-then-ack survives an *ungraceful Core crash*; B2b/1 proves the same durability + a live-through-it liveness survives a *graceful DB bounce*. The single permanent guard the slice leaves is the **3-test ack classifier** (`internal` → `error_acks`, `rate_limited` → `rate_limited`, ok → neither): if a rate-limit were miscounted as an error ack, the `assert_error_acks` gate would pass **vacuously** under normal rate-limiting with no outage at all — so the classification is exactly the thing that must not silently drift. The adversarial-lens ritual had nothing else to bite on — no new branch/parser/money path beyond that tiny classifier — correctly, per ponytail; none was manufactured.

### Verification (rule 4)

- `cargo fmt -p opn-loadgen --check` clean; `cargo clippy -p opn-loadgen --all-targets -- -D warnings` clean (the new field, gate, and classifier branch lint-clean; the `core` crate's `unwrap_used` deny untouched — loadgen changes are counters + `matches!`).
- **9 loadgen unit tests green** (6 pre-existing + the 3 new ack-classifier guards).
- **`pg-restart.sh` run three consecutive times against the real stack** (docker compose: Postgres 16 + Redis 7 + MinIO, release Core, `stop postgres` / `sleep 6` / `start` orchestration on the dev host, i5-14500): each run **54 acked message(s) survived the 6 s DB outage and all replayed, exit 0**, and `assert_error_acks` passed every run (loadgen exited 0 with the gate set ⇒ `error_acks > 0` — else the drill's FAIL branch fires). The full pipeline exercised end to end: journal write → outage → in-gap error acks → recovery → resume-from-0 → drain 54 replayed seqs → compare.
- Drift gate untouched — `error_acks` is a loadgen-internal summary field, not a contracts wire type; no `.d.ts` changed.

### Exit criteria status (Sprint 9 — B2b/1 slice; B2b/2+3 = the other two drills still open)

| Criterion (roadmap Sprint 9) | Status |
|---|---|
| All four proptest suites green at 1024 cases locally | **CLOSED (B1)**. |
| 24 h fuzz per target, zero crashes | **PARTIAL → strong (B1)** — targets build + smoke clean; 24 h burn-in is an operator out-of-band run. |
| `just chaos` green three consecutive runs; weekly CI | **PARTIAL → stronger** — `kill9-mid-send` (B2a) + `pg-restart` (this slice) both green three consecutive runs, both in `just chaos` + weekly `chaos.yml`; `redis-restart`/`link-drop` not built yet, so `just chaos` runs **two of four**. |
| Generated RLS test covers every world_id table | **CLOSED (part A)**. |
| Dependency audit gate (`cargo deny`) | **CLOSED (part A, pending first-CI tune)**. |

### Reflection

- **The new-machinery seam was the right fourth cut.** pg-restart is a thin script on B2a's finished `lib.sh` + verifier + one bounded, pre-flagged loadgen touch — nothing new stood up. The two remaining drills each need genuinely new orchestration (a second Core instance; a fake `/link` consumer), so they belong in their own slices, exactly the clean split the project has taken at every boundary since Sprint 4.
- **A chaos slice's success looks like silence — and this one added a second kind of confidence.** No keeper, no fix: kill9 confirmed durability survives an ungraceful crash; pg-restart confirms durability *and* liveness survive a graceful DB outage (Core answers `internal` through the gap and recovers with zero code). Both are the §15 budget working; ADR-1 buys the confirmation as much as the catch.
- **Ponytail held on the touch and the harness.** One unconditional counter over a gated one; a typed scenario gate over a bash JSON-grep; the verifier reused verbatim over a second copy; a deterministic stop-gap over a flaky `restart`; the PASS string generalized once two callers share it. Smallest diff that actually enforces the property.

### Not committed / next session

- **Sprint 9 part B2b/1 is complete and green but untracked** on top of committed 0–8 + part A + part B1 + part B2a. First commit lands the loadgen `error_acks` counter + `assert_error_acks` gate (`driver.rs`/`main.rs` + 3 unit tests), the generalized verifier PASS string (`verify.rs`), `pg_restart_gap` (`chaos/lib.sh`), `chaos/pg-restart.sh`, `scenarios/chaos-pg.json`, and the `just chaos` + `chaos.yml` wiring — the operator's call, as every sprint. Drift gate untouched.
- **Next: the two remaining chaos drills.** `redis-restart` (restart Redis under **two-instance mode** → presence keys rebuild within one 30 s heartbeat + the pub/sub listener resubscribes; needs a second Core instance in `lib.sh` and a `redis-cli`/cross-instance-delivery check — new orchestration) then `link-drop` (a fake `/link` WS consumer driving a call to `active`, dropped mid-call → reconnect → `GET /v1/tenants/self/calls/active` re-sync returns the call and a subsequent accept re-emits targets — needs the link-consumer tool). Each extends `just chaos` + `chaos.yml`. After both, Sprint 9 is closed and the project moves to Sprint 10 (performance & soak).
- Minor/non-blocking: the acked count is still rate-bucket-bound (~54 over the 25 s window — a solid non-vacuous batch); the `cargo deny` license allow-list still wants its first-CI pass (part A); the 24 h fuzz burn-in remains an operator out-of-band run.

---

## 2026-07-19 — Sprint 9 **part B2a** (chaos, kill9 slice: the loadgen ack-journal extension + a `--verify-resume` mode + `kill9-mid-send.sh` on a reusable `chaos/lib.sh` harness + `just chaos` + the weekly chaos CI job): built, verified live (three consecutive green drill runs); the three restart/drop drills (part B2b) deferred

Part B1 closed the out-of-band **fuzz** half and named part B2 as "the chaos scripts + the loadgen acked-id verifier + `just chaos` + the weekly CI job", flagging **loadgen recording acked `(message_id, seq)` ids** as "the load-bearing prerequisite". That prerequisite is exactly the seam B2 splits on: of the four drills, **only `kill9-mid-send` needs the loadgen surgery** — the other three (`pg-restart`, `redis-restart`, `link-drop`) reuse the same stack-orchestration harness but inject a different fault and assert a different invariant, none of which touches loadgen's ack path. So B2 splits again on the **loadgen-extension seam**: **(B2a)** the kill9 slice — the ack-journal extension, the resume verifier, the one drill, and the reusable `chaos/lib.sh` every later drill sits on; **(B2b)** the three fault-injection drills that only add "kill X mid-load → check invariant Y" on top of the finished harness. B2a is the self-contained, fully-locally-verifiable slice (docker + a release Core + kill-9 orchestration all ran on the dev host); B2b is a different fault set and its own session. No roadmap amendment — B2a is roadmap item 3's `kill9-mid-send` bullet + the "loadgen records acked ids; verifier compares" clause, verbatim.

### What exists now

- **loadgen ack journal** (`crates/loadgen/src/driver.rs`, `main.rs`) — `ConnStats` gains `acked_seqs`, `channel_id`, `token`; on every **ok** ack for a tracked `channels.send` the connection records the payload's `seq` (persist-then-ack means an ok ack ⇒ the row is committed, so the acked seq *is* the must-survive set). Gated on `ConnConfig::record_acks`, itself set only when `OPN_LOADGEN_ACK_JOURNAL` is present — the perf smoke and the Sprint 10 soak pay **nothing** for it. `main::write_journal` collapses the two pair members' seqs into one entry per channel (`{channel_id, token, acked_seqs}`) and writes it before `Summary::merge` consumes the results.
- **`crates/loadgen/src/verify.rs`** (new) — `opn-loadgen --verify-resume <journal> <ws_url>`: for each journaled channel, connect + auth with the stored member token, `sub ch:<id> last_seq=Some(0)` (replays every committed message), drain the `channels.message` pushes *until the sub ack* (snapshot-before-ack, §4.4), and assert every acked seq replayed. Exit 0 all present, 1 a gap **or zero acks recorded** (an empty journal is a failed drill, not a vacuous pass), 2 an op error — the same code convention as the load run. A `resume_overflow` (500-row page) fails loud ("shorten the run") because the verifier then can't see the whole set.
- **`chaos/lib.sh`** (new, reusable) — the harness every drill sits on: compose up (with a `down -v` first, so runs are repeatable — `create-tenant` refuses an existing world), release build, background Core + healthz wait, `core_kill9`/`core_stop`, `mint_tenant` (reuses the `admin create-tenant` path perf-smoke.yml already drives), and an EXIT trap that stops Core and tears the stack down. Core config mirrors `perf-smoke.yml`, all env-overridable.
- **`chaos/kill9-mid-send.sh`** (new) — loadgen at 30 msg/s journaling acks → `sleep 9` → `kill -9` Core mid-stream → restart → `--verify-resume` is the sole gate (loadgen's own post-kill socket errors are expected and ignored).
- **`crates/loadgen/scenarios/chaos-kill9.json`** — 2 conns (one pair, one channel), 30 msg/s, warmup 3 s, no perf gates (the verifier gates, not p99).
- **`justfile`** — `just chaos` (runs `kill9-mid-send.sh`; documents the `sg docker -c 'just chaos'` wrapper the dev host needs). **`.github/workflows/chaos.yml`** — weekly (`Mon 04:00 UTC`) + `workflow_dispatch`, a separate file from `perf-smoke.yml`/`nightly-verify.yml` (§15: "run in CI weekly, not a one-time manual exercise").

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **A separate `--verify-resume` subcommand, not reconnection logic baked into the driver.** The alternative — loadgen itself detects the kill, reconnects, and re-subs — is heavier and races the kill timing. A verifier that runs *after* the restart is confirmed healthy is deterministic: the drill either recorded acks and they replay, or it didn't. The verifier reuses the driver's `send`/`await_ack` (promoted to `pub(crate)`); only the sub-drain loop is custom, because `await_ack` skips pushes and the resume replay *is* pushes.
2. **The journal carries a member token.** The pair channels are between loadgen's own minted characters, so a fresh mint isn't a member and couldn't `sub`. Storing one pair member's JWT lets the verifier resume as a real member. A short-lived token in a local test-harness file is fine (the whole drill runs well inside the 10-min JWT TTL).
3. **`record_acks` gated on the journal env var.** Off in every perf/soak scenario, so the unbounded-`Vec` concern the loadgen module header already flags for the 24 h soak simply never arms here.
4. **Acks survive the kill for free — a pre-existing property made load-bearing.** The drive loop `break`s (does not `Err`) on a dead socket and returns `Ok(stats)`, so a mid-run `kill -9` keeps the acks collected before it; only *setup* failures discard stats, and the warmup barrier guarantees the kill lands after setup. No new resilience code — the extension just names an existing guarantee.
5. **`stack_up` does `down -v` first.** The "green three consecutive runs" exit criterion needs a clean DB each run; destructive to a running dev stack by design (the drill owns the stack for its duration, same as the CI cleanup already does).

### The keeper this session: an empirical cap, not a code defect — and that is the expected shape for a chaos slice

Like B1's fuzz slice, B2a added **no new production logic** — it added a fault-injection net over existing delivery guarantees. The one genuinely interesting finding was **empirical**: the drill acks a stable **~20 messages** regardless of how wide the pre-kill window is opened (6 s → 9 s changed nothing), because the per-character `channels.send` rate bucket caps throughput far below the 30 msg/s the two senders attempt. This is **not a bug** — the invariant only asserts that every *acked* message survives, and 20 is a solid, non-vacuous batch. But it drove the one real tuning decision: widen the `sleep` from 6 s to 9 s so the **empty-journal FAIL path** (a genuine guard — zero acks recorded means the drill proved nothing) can never trip on CI timing jitter, even though the acked *count* is bucket-bound either way. The single permanent guard the slice leaves is the `missing_seqs` unit test: a dropped seq must be reported, so the drill's verdict can't silently go vacuous. The adversarial-lens ritual had nothing to bite on — no new branch/parser/money path — correctly, per ponytail, none was manufactured.

### Verification (rule 4)

- `cargo fmt --check` clean; `cargo clippy -p opn-loadgen --all-targets -- -D warnings` clean (the two `pub(crate)` promotions and the three new `ConnStats` fields lint-clean; the `core` crate's `unwrap_used` deny is untouched — `verify.rs` uses `let … else`/`if let`/`bail!`, no unwrap).
- **6 loadgen unit tests green** (the 4 pre-existing + the 2 new `missing_seqs` guards).
- **`kill9-mid-send.sh` run three consecutive times against the real stack** (docker compose: Postgres 16 + Redis 7 + MinIO, release Core, `kill -9` + restart orchestration on the dev host, i5-14500): each run **20 acked message(s) survived `kill -9` and all replayed, exit 0**. The full pipeline exercised end to end — journal write → connect → auth → resume-from-0 → drain 20 real replayed seqs → compare — so the pass is not vacuous.
- Drift gate untouched — the extension only *reads* the `seq`/`message_id` the `channels.send` ack already carries; no wire type changed.

### Exit criteria status (Sprint 9 — B2a slice; B2b = the other three drills still open)

| Criterion (roadmap Sprint 9) | Status |
|---|---|
| All four proptest suites green at 1024 cases locally | **CLOSED (B1)**. |
| 24 h fuzz per target, zero crashes | **PARTIAL → strong (B1)** — targets build + smoke clean; the 24 h burn-in is an operator out-of-band run. |
| `just chaos` green three consecutive runs; weekly CI | **PARTIAL → strong** — `kill9-mid-send` green **three consecutive runs**, `just chaos` wired, weekly `chaos.yml` scheduled; `pg-restart`/`redis-restart`/`link-drop` (B2b) not built yet, so `just chaos` runs one of four. |
| Generated RLS test covers every world_id table | **CLOSED (part A)**. |
| Dependency audit gate (`cargo deny`) | **CLOSED (part A, pending first-CI tune)**. |

### Reflection

- **The loadgen-extension seam was the right third cut.** B2a is the *only* chaos slice that needed to touch loadgen; doing it alone both discharged the prerequisite B1 flagged and delivered `chaos/lib.sh` — the compose+Core+mint+teardown harness — so B2b's three drills reduce to "inject fault X, assert invariant Y" with no new plumbing. Same clean split the project has taken at every sprint boundary since 4.
- **A chaos slice's success looks like silence too.** No production keeper, no fix — three green drill runs proving persist-then-ack + resume actually survive a hard crash. B1 confirmed the untrusted-input contract; B2a confirms the durability contract. Both are the §15 budget working; ADR-1 buys the confirmation as much as the catch.
- **Ponytail held on the verifier and the harness.** A separate deterministic verifier over driver-baked reconnection; a token in the journal over token-machinery; `record_acks` gated so perf/soak pay nothing; driver helpers reused via `pub(crate)` over a duplicated WS dance; one reusable `lib.sh` over four copy-pasted orchestration preambles. Smallest diff that actually enforces the property.

### Not committed / next session

- **Sprint 9 part B2a is complete and green but untracked** on top of committed 0–8 + part A + part B1. First commit lands the loadgen ack-journal extension (`driver.rs`/`main.rs`), `verify.rs`, `chaos/lib.sh` + `chaos/kill9-mid-send.sh`, `scenarios/chaos-kill9.json`, the `just chaos` recipe, and `.github/workflows/chaos.yml` — the operator's call, as every sprint. Drift gate untouched.
- **Next: Sprint 9 part B2b — the three remaining chaos drills**, each a thin script on the finished `chaos/lib.sh`: `pg-restart` (restart Postgres under load → pool reconnects, zero acked-but-lost via the same `--verify-resume`, **error acks not silence** during the gap — the one that needs a bounded loadgen touch to surface acks-arriving-during-the-gap), `redis-restart` (presence keys rebuild within one heartbeat + pub/sub resubscribes, two-instance mode), `link-drop` (kill the fake link consumer mid-call → reconnect → re-sync returns the active call and a subsequent accept emits targets). Each extends `just chaos` and `chaos.yml`. After B2b, Sprint 9 is closed and the project moves to Sprint 10 (performance & soak).
- Minor/non-blocking: the acked count is rate-bucket-bound (~20/drill) — fine for the invariant, but if a future drill wants higher volume it needs multiple channels, not a higher rate; the `cargo deny` license allow-list still wants its first-CI pass (part A); the 24 h fuzz burn-in remains an operator out-of-band run.

---

## 2026-07-19 — Sprint 9 **part B1** (out-of-band half, fuzz slice: three `cargo-fuzz` targets + committed seed corpus + nightly fuzz/proptest CI + the 1024-case proptest burn-in): built, verified live; chaos drills (part B2) deferred

Part A closed the *in-suite* verification net (proptest + RLS audit + `cargo deny`) and named part B as "the out-of-band harnesses: `cargo-fuzz` targets **and** the four chaos scripts". Those two are a different kind of work still — libFuzzer needs nightly + a sanitizer build; the chaos drills need kill-9 orchestration against the compose stack and a loadgen acked-id verifier — so part B splits again on the **toolchain seam**: **(B1)** the fuzz targets + corpus + the nightly CI cadence (fuzz smoke + the 256-case proptest run) + the one-time 1024-case local burn-in; **(B2)** the chaos scripts (`kill9-mid-send`, `pg-restart`, `redis-restart`, `link-drop`) + the loadgen verifier + `just chaos` + the weekly CI job. B1 is a self-contained, fully-locally-verifiable slice (nightly 1.99 + cargo-fuzz are both present on the dev host, so the targets were actually built and run, not just written); B2 is genuinely different (docker crash choreography, a loadgen extension) and belongs in its own session. No roadmap amendment — B1 is roadmap item 2 + the "wire nightly proptest / do the 1024 burn-in" line from item 1, verbatim.

### What exists now

- **`opn-core/fuzz/`** — a `cargo-fuzz` crate with its **own empty `[workspace]`** so the nightly+ASan build never pulls into the stable workspace next door (the parent lists explicit members and never referenced `fuzz/`). Three targets, each the roadmap's named surface:
  - **`fuzz_client_frame`** — `serde_json::from_slice::<ClientFrame>` over arbitrary bytes (the primary attacker-controlled surface), then on a successful parse the *synchronous, DB-free* pre-handler validation `gateway::dispatch::run` runs before any handler: `TopicKind::parse` on `Sub`/`Unsub`, `channels::validate_body` on `ChannelsSend`, `feed::validate_doc` on `FeedPost`/`FeedComment`. Crash = bug (dispatch's contract is "a bad frame becomes an ack, never a panic").
  - **`fuzz_link_hello`** — `serde_json::from_str::<LinkHello>` on UTF-8 (the exact shape `gateway/link.rs` parses from a WS Text frame); a bad hello is a clean `BAD_HELLO` close, never a panic.
  - **`fuzz_cursor_decode`** — `cursor::decode` (the base64url → JSON → `OffsetDateTime` chain, the classic panic trap); the same never-panic property `prop_cursor.rs` proves generatively, proven exhaustively here.
- **Committed seed corpus** (roadmap "corpus committed"): 6 valid `ClientFrame` JSONs spanning the validated arms (sub-ch/sub-feed/unsub/channels.send/feed.post + the unit `auth.refresh`), one `LinkHello`, one real encoded cursor. Only the hand-named seeds are tracked; the fuzzer-grown SHA1 expansion (thousands of files) is not committed — regenerable, machine-specific bloat.
- **Two internal validators made `pub`** — `channels::validate_body` and `feed::validate_doc` — so the fuzz target drives the *real* body-cap code, not a re-derived copy (no drift). Both are pure (serde size + host-allowlist parse); the fuzz value is proving they never panic on arbitrary parsed bodies.
- **`justfile`** — `just fuzz [secs]` (smoke every target for `secs`, default 60; `just fuzz 300` = the CI 5-min burn) and `just proptest [cases]` (the property suites at full case count, default 1024 — the exit-criterion run).
- **`.github/workflows/nightly-verify.yml`** (new, nightly `cron` + `workflow_dispatch`, separate from `perf-smoke.yml` — different concern): a `proptest` job (`PROPTEST_CASES=256`, the four suites, full Postgres+Redis+MinIO stack) and a `fuzz` matrix job (one leg per target, nightly toolchain, `cargo +nightly fuzz run … -max_total_time=300`, crash artifacts uploaded on failure). The in-suite 16-case proptest already gates every push via `ci.yml`'s `test` job; this adds the deep nightly cadence §15 asks for.

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **`fuzz_client_frame` covers the pure pre-handler path only** (parse + topic parse + body caps), not the full handler. Body-cap validation is the deepest synchronous, DB-free validation dispatch runs; everything past it (membership, media ownership, seq) needs Postgres and is already the DB-backed proptests' job (`prop_channels`/`prop_ledger`). Exposing the two size validators as `pub` was the minimal way to fuzz the *real* code — the alternative, re-deriving the size check in the target, drifts the moment `BODY_MAX_BYTES` changes.
2. **`fuzz/` is its own workspace, not a fourth member.** cargo-fuzz's sanitizer/nightly build is incompatible with the stable pinned toolchain the workspace uses; a detached workspace keeps `cargo build`/`clippy`/`test` on the main tree untouched and lets `cargo +nightly fuzz` own its own lockfile and target dir. `+nightly` on the command line overrides `rust-toolchain.toml`'s stable pin for the dir — documented inline in the CI job.
3. **A dedicated `nightly-verify.yml`, not more jobs bolted onto `perf-smoke.yml`.** Fuzz+proptest are verification, not perf; they want a different toolchain (nightly) and a different failure story (a crash artifact, not a latency gate). One extra file reads far clearer than overloading the perf workflow's name.
4. **256-case nightly / 1024-case burn-in split** exactly per the roadmap ("nightly at 256, one-time 1024 local"). The DB-backed suites scale linearly in `PROPTEST_CASES` (they read it manually inside the async `#[sqlx::test]`, per part A's decision 1), so the same suites cover both cadences with one env knob — no separate "heavy" suite.

### The keeper this session: none in the code, and that is the expected shape for a fuzz slice

Unlike every DB-backed sprint since 4, B1 added **no new production logic** — it added a net over existing code. The three targets ran millions of executions against the parse + validation surface and found **zero crashes**, which is the *confirming* half of the §15 budget, not a null result: the untrusted-input contract ("dispatch/link/cursor never panic on arbitrary bytes") is now continuously enforced, not merely asserted by a handful of hand-picked `garbage_is_invalid` unit cases. The one code change (two `fn → pub fn`) is API surface, not behavior. The adversarial-lens ritual that caught a keeper every prior sprint had nothing to bite on here because there is no new branch/loop/parser — correctly, per ponytail, no lens was spun up to manufacture one.

### Verification (rule 4)

- `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean (the two `pub` promotions add no lint; `unwrap_used` deny untouched — the targets use `let … else`/`if let`, no unwrap).
- **`cargo +nightly fuzz build` — exit 0**, all three targets compile under ASan (opn-core builds DB-free: zero compile-time `sqlx::query!` macros in the tree, so no `DATABASE_URL`/offline-data needed for the sanitizer build).
- **All three targets smoke-run clean on the dev host** (i5-14500, nightly 1.99): `fuzz_cursor_decode` 5.5M execs @ ~458k/s; `fuzz_client_frame` DONE cov 2151, ~4.1M execs; `fuzz_link_hello` DONE cov 1177, ~11M execs. **Zero crashes, zero artifacts.** (Smoke = 20–60 s/target locally; the 5-min nightly and the one-time 24 h burn-in run in CI/out-of-band.)
- **The 1024-case proptest burn-in — green** against the live stack: `prop_fsm` 7, `prop_cursor` 3, `prop_ledger` 2, `prop_channels` 2 — 14 tests, 0 failures, ~90 s wall (the DB-backed pair ~22 s + ~67 s at 1024 cases; the pure pair sub-second). This closes the exit criterion part A left PARTIAL.
- Drift gate untouched — verification adds no wire types (the `pub` promotions are core-internal, not contracts).

### Exit criteria status (Sprint 9 — B1 slice; B2 = chaos still open)

| Criterion (roadmap Sprint 9) | Status |
|---|---|
| All four proptest suites green at 1024 cases locally | **CLOSED** — 14 tests green at `PROPTEST_CASES=1024` on the dev host; nightly runs the same suites at 256 via `nightly-verify.yml`. |
| 24 h fuzz per target, zero crashes | **PARTIAL → strong** — the three targets exist, build under ASan, and smoke clean at millions of execs each; the nightly 5-min-per-target job is wired; the *one-time* 24 h burn-in is the operator's out-of-band run (a CI/soak action, not a code deliverable). |
| `just chaos` green three consecutive runs; weekly CI | **OPEN (B2)** — chaos scripts + loadgen verifier not built yet. |
| Generated RLS test covers every world_id table | **CLOSED (part A)**. |
| Dependency audit gate (`cargo deny`) | **CLOSED (part A, pending first-CI tune)**. |

### Reflection

- **The toolchain seam was the right second cut.** B1 is `cargo +nightly fuzz` + a nightly CI YAML — nothing needs a running Postgres to *crash* or a kill-9 to *orchestrate*; it reused the exact wire types and pure validators already in the tree, so the whole slice is one detached crate + one workflow + a two-word visibility change. B2 (chaos) is a different toolset (docker choreography, a loadgen acked-id verifier) and a different failure signal, exactly the kind of clean split the project has taken at every sprint boundary since 4.
- **A verification slice's success looks like silence, and that is the point.** No keeper, no fix — millions of adversarial executions and a 1024-case burn-in that all came back green. Part A's generative layers *found* things (an over-tight invariant, an un-encoded RLS exception); B1's fuzz layer *confirmed* the untrusted-input contract holds. Both are the §15 budget working; ADR-1 buys the confirmation as much as the catch.
- **Ponytail held on the corpus and the workspace.** Seed corpus committed, fuzzer expansion discarded (regenerable); one detached workspace instead of contorting the pinned stable toolchain; the real validators reused via `pub` instead of a drift-prone copy. Smallest diff that actually enforces the property.

### Not committed / next session

- **Sprint 9 part B1 is complete and green but untracked** on top of committed 0–8 + part A. First commit lands `opn-core/fuzz/` (crate + 3 targets + seed corpus), the `nightly-verify.yml` workflow, the two `justfile` recipes, and the two `pub` promotions — the operator's call, as every sprint. Drift gate untouched.
- **Next: Sprint 9 part B2 — the chaos drills.** The four scripts in `chaos/` (`kill9-mid-send`: loadgen at 30 msg/s → `kill -9` core → restart → every acked message replayed to a resuming client; `pg-restart`, `redis-restart`, `link-drop`), each asserting its invariant by exit code, all under `just chaos`, plus the weekly CI job (§15). The load-bearing prerequisite: **loadgen records acked `(message_id, seq)` ids** for the kill9 verifier to compare against post-restart replay — a bounded extension to `crates/loadgen` (it currently keeps only RTT/delivery histograms, not the acked-id set). That, and the docker crash orchestration, is the whole of B2. After B2, Sprint 9 is closed and the project moves to Sprint 10 (performance & soak).
- Minor/non-blocking: the `cargo deny` license allow-list still wants its first-CI pass (part A); the 24 h fuzz burn-in is an operator out-of-band run, not blocking B2.

---

## 2026-07-19 — Sprint 9 **part A** (verification hardening, in-suite half: proptest suites for the four invariants + generated per-table RLS audit + `cargo deny` gate): built, verified live; fuzzing + chaos drills (part B) deferred

Sprint 9 is the stability-grade verification layer (OPN-CORE.md §15). It splits cleanly on a
**delivery-mechanism seam**, not a primitive seam: **(A)** everything that lives *inside* `cargo test`
and CI config — the four property-test suites (roadmap item 1), the generated per-table RLS audit
(item 4), and the `cargo deny` licenses/advisories gate (item 5); **(B)** the *out-of-band harnesses*
that need a separate toolchain or a running stack — `cargo-fuzz` targets (item 2, nightly Rust +
its own crate) and the chaos drill scripts (item 3, kill-9 / restart orchestration against the compose
stack + a loadgen verifier). Part A is the generative + audit net that then runs forever in CI; part B
is the crash/fuzz surface. No roadmap amendment — this is items 1/4/5 verbatim.

### What exists now

- **`crates/core/tests/prop_fsm.rs`** (7 properties, pure — full `proptest!` shrinking, no DB). Over the
  calls FSM (`calls::fsm::apply`) and the hold FSM (`ledger::fsm::apply`): totality (never panics on any
  generated `session × actor × others × action`), terminal absorption (`Ended`/`Captured`/`Released`
  reject everything), **legality vs an independently-encoded predicate** (the legal set written once as a
  predicate, NOT by calling `apply` — that would mirror the impl and prove nothing), result-state
  legality, the session-end rule (Decline/Hangup end iff no other party stays active), and hold stream
  absorption from `Held`.
- **`crates/core/tests/prop_cursor.rs`** (3 properties). Microsecond-exact encode→decode round-trip;
  arbitrary strings and arbitrary bytes decode to `Ok` or `Fail::Code(Invalid)` and **never panic** (the
  §15 fuzz-target property, proven generatively here too).
- **`crates/core/tests/prop_ledger.rs`** (2 tests, real Postgres). Generated op sequences
  (`Transfer|Hold|Capture|Release`, indices/amounts from small pools, hold refs resolved at runtime).
  *Sequential* asserts all four ledger invariants after each sequence — per-account
  `balance == Σ transfers` via the **same `store::reconcile` SQL prod freezes on** (no drift), `Σ balances
  == 0`, no negative wallet, `available ≥ 0` per wallet. *Concurrent* splits transfers across 8 tasks and
  asserts only the global invariants — the deadlock-free `FOR UPDATE … ORDER BY id` order under
  contention (zero `Fail::Internal`), generalizing the Sprint 7 concurrency battery.
- **`crates/core/tests/prop_channels.rs`** (2 tests, real Postgres). Generated send streams whose
  client_uuids collide (drawn from a 6-slot pool). *Sequential*: repeat uuids dedupe to an identical
  `(message_id, seq)` ack, first sends don't; seq is a gapless `1..=distinct`; `last_seq == distinct ==
  count(*)`. *Concurrent*: one task per send — the channel row lock must keep dedup consistent and seq
  gapless under races.
- **`crates/core/tests/rls_audit.rs`** (generated, catalog-driven). Enumerates every `public` table with a
  live `world_id` column straight from `pg_class`/`pg_attribute` (partition *children* excluded — the
  partitioned parent `messages` governs), and asserts each ENABLEs + FORCEs RLS and carries a
  `world_id`-referencing policy. **28 world-scoped tables** covered. A `tenants`-shaped documented
  `INFRA_EXCEPTIONS` allowlist (world_id column, but GRANT-guarded infra row, not RLS — roadmap Sprint 1),
  self-checked against staleness; plus a floor-count guard so a broken enumeration can't pass vacuously.
- **`opn-core/deny.toml`** + a **`cargo-deny` CI job** (`.github/workflows/ci.yml`): advisories (+ yanked)
  and an SPDX license allow-list, source-registry lockdown, duplicate-version warn. `proptest = "1"` added
  to core dev-deps.

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **proptest as a *generator* inside the async `#[sqlx::test]`, not the sync `proptest!` runner, for the
   DB-backed suites.** `strategy.new_tree(&mut runner).current()` yields a value; the async test executes
   it with plain `.await` against one pool — no runtime-nesting hack (`Handle::block_on` inside a runtime
   panics). Cost: no automatic shrinking. Bought back with `TestRunner::deterministic()` (a red CI
   reproduces byte-for-byte) and print-the-whole-sequence-on-fail. The roadmap already makes manual
   minimization the intended path ("the generative layer finds, the deterministic layer remembers"), so
   the trade is free. The *pure* FSM/cursor suites have no DB and DO use the full `proptest!` macro with
   shrinking. Marked `ponytail:` — add async-shrinking machinery only if a real failure ever resists
   hand-minimization.
2. **Fresh RLS-isolated world per proptest case, no inter-case cleanup.** Every invariant query runs in
   `world_tx(case_world)`, so cases sharing one `#[sqlx::test]` database cannot contaminate each other —
   RLS is the isolation the test relies on, tested by relying on it.
3. **The RLS audit is a catalog *mechanism* proof, not per-table behavioral seeding.** It asserts
   ENABLE+FORCE+world_id-policy for every world_id table — exactly the exit criterion's "diffing
   `information_schema`", non-vacuous (a new table that forgets RLS fails), and generic (no per-table
   fixture). The *behavioral* two-world proof stays where it already is: `rls_canary` + each primitive's
   own cross-world test. The generated audit is the coverage net over them.
4. **Concurrent ledger variant is transfer-only.** Holds/captures don't parallelize meaningfully without
   shared runtime state to track live hold ids; the sequential variant already carries the full
   hold/capture/release FSM interplay. This is exactly the roadmap's "split the sequence across 8 tasks,
   assert only the global invariants."

### The keepers this session (the point of rule 4): both generative layers caught something on run one

- **The ledger proptest failed on case 0 — and the bug was in the *test*, which is itself the signal.** My
  `available ≥ 0` invariant flagged the **system account's** by-design negative balance (it is the mint;
  `CHECK (balance >= 0 OR owner_kind = 'system')` explicitly lets it go negative). An imprecise invariant
  is a real defect — a too-strict property gives false confidence when it's *right* and noise when it's
  *wrong*. Fixed by scoping the wallet checks to `owner_kind = 'character'`. Conservation (`reconcile`
  clean + `Σ = 0`) passed untouched — the money code is correct; my statement of the invariant wasn't.
- **The RLS audit surfaced `tenants` on run one — the single world_id table that is *deliberately* not
  RLS-protected.** That is the audit doing its job: it found the exact exception to the rule and forced it
  to be encoded explicitly and documented (GRANT-guarded infra row, roadmap Sprint 1), instead of a
  hand-written canary silently never covering it. A blanket "every world_id table forces RLS" would have
  been *wrong*; the catalog made the exception visible and named.

### Verification (rule 4)

- `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean (fixed on the way:
  `doc_lazy_continuation` on the numbered-list doc comments, one redundant `prelude` import, one dead
  struct field — the `unwrap_used` deny respected, `.expect`/`prop_assert!` only).
- **15 new tests green against the live stack** (Postgres+Redis+MinIO): 7 (`prop_fsm`) + 3 (`prop_cursor`)
  + 2 (`prop_ledger`) + 2 (`prop_channels`) + 1 (`rls_audit`). Robust at `PROPTEST_CASES=200` (ledger
  seq ~11 s, channels seq ~4 s). RLS audit reports 28 world-scoped tables all FORCE RLS.
- **`cargo deny` not run locally** (not installed; a heavy `cargo install`). `deny.toml` + `ci.yml` parse;
  the license allow-list is a best-effort SPDX set — **the first CI run may need one or two additions**
  once it sees the real tree. Flagged for the operator; the gate is designed to be tuned on first sight.

### Exit criteria status (Sprint 9 — partial; part-A items only)

| Criterion (roadmap Sprint 9) | Status |
|---|---|
| All four proptest suites green at 1024 cases locally | **PARTIAL** — all four suites exist and are green at 200 cases; the 1024-case local burn-in is a dedicated run (env `PROPTEST_CASES=1024`), do it alongside the part-B nightly wiring. |
| 24 h fuzz per target, zero crashes | **OPEN (part B)** — `cargo-fuzz` targets not built yet. |
| `just chaos` green three consecutive runs; weekly CI | **OPEN (part B)** — chaos scripts not built yet. |
| Generated RLS test covers every world_id table | **CLOSED** — `rls_audit.rs` enumerates from the catalog (28 tables), FORCE-RLS + policy asserted, `tenants` the one documented exception; floor-count + stale-allowlist guards keep it honest. |
| Dependency audit gate (`cargo deny`) | **CLOSED (pending first-CI tune)** — `deny.toml` + CI job landed; local run skipped, allow-list may need one pass. |

### Reflection

- **The mechanism seam was the right cut.** Part A is entirely `cargo test` + CI YAML — nothing needs
  nightly Rust or a choreographed crash. It reused every existing harness idiom (`app_pool`,
  `seed_world_tenant`, `world_tx`, `mint_session`, the ledger `reconcile`/`fund` helpers) so the four
  suites are strategy + invariant, no new machinery. Part B is a genuinely different kind of work (a fuzz
  crate, kill-9 orchestration) and belongs in its own session.
- **Generative testing paid for itself before it found a code bug.** Both new generated layers caught
  something on their first run — an over-tight invariant and an un-encoded RLS exception. Neither was a
  production defect; both were *gaps in how correctness was stated*, which is exactly what the §15
  hardening budget is meant to surface. The confidence half of ADR-1: the money and RLS code came back
  clean under generated adversarial sequences, and the fixes were all in the tests.
- **The two easy, independent chunks (pure FSM/cursor proptests; the `cargo deny` config) were delegated
  to parallel subagents** while the DB-backed proptests + the RLS audit — the parts needing the schema and
  harness in-head — were done on the main thread. Clean division: the shared `Cargo.toml`/`ci.yml` edits
  stayed on one owner to avoid races.

### Not committed / next session

- **Sprint 9 part A is complete and green but untracked** on top of committed 0–8. First commit lands the
  five new `tests/*.rs`, `deny.toml`, the `cargo-deny` CI job, and the `proptest` dev-dep. Drift gate
  untouched (no contracts change — verification adds no wire types).
- **Next: Sprint 9 part B — the out-of-band harnesses.** `cargo-fuzz` targets (`fuzz_client_frame` over
  `ClientFrame` parse + the validation layer, `fuzz_link_hello`, `fuzz_cursor_decode`; corpus committed,
  5 min/target nightly, `just fuzz` local); the four chaos scripts (`kill9-mid-send`, `pg-restart`,
  `redis-restart`, `link-drop`) each asserting their invariant by exit code, `just chaos`, weekly CI job;
  wire the nightly proptest run at `PROPTEST_CASES=256` and do the one-time 1024-case local burn-in.
- Minor/none-blocking: `cargo deny` license allow-list wants a first-CI pass; the concurrent proptest case
  counts are deliberately capped (4/6) to bound wall-clock — the sequential suites carry the case-count
  scaling via `PROPTEST_CASES`.

---

## 2026-07-19 (Sprint 8B) — Sprint 8 **part B** (feed read surface: home fan-out-on-read, profile, post detail + comments, hashtag page — HTTP cursor idiom + 100 k EXPLAIN gate + p95): built, verified live; **Sprint 8 complete**

Sprint 8 split on the write-plane/read-plane seam (part A did the writes on part A's committed schema).
Part B is the **read plane**: the four HTTP timelines the "fan-out-on-read" headline names, plus the two
exit criteria part A explicitly deferred to here — the 100 k-row EXPLAIN gate and the recorded p95. No
roadmap amendment: this is roadmap items 3 + the read half of item 4, unchanged.

### What exists now

- **`primitives/feed/read.rs`** (new, read-plane store). Four fns on the shared cursor idiom (CDR-7),
  every one inside `world_tx(who.world_id)` (RLS): `home` (the fan-out-on-read `EXISTS` timeline — self
  posts OR followed authors, on `posts_home`), `profile` (one author, on `posts_author`), `post_detail`
  (the post + a keyset comment page on `comments_post`), `hashtag` (posts under a tag, joined via the
  `hashtags` PK). `PostRow`/`CommentRow` → `PostItem`/`CommentItem` via `From` (the `i32` counters widen
  to `i64`, `created_at` → RFC 3339). Reused `ledger`'s `cursor_binds`/`cursor::page`/`rfc3339` — no new
  idiom minted.
- **Two authorization shapes**, mirroring part A's write-vs-sub split: `home` is *my* feed, so it resolves
  the caller's **active** account (`active_account`, `forbidden` if not logged into the app); `profile`/
  `post_detail`/`hashtag` don't act as an account, so they gate on `require_app_member` (owns *an* account
  for the app — the same rule `authorize_sub` uses).
- **`http/feed.rs`** (new): four JWT-authed handlers + `FeedQuery` (`app_id?`, `cursor?`, `limit?`).
  `FeedQuery::parts` decodes the cursor (garbage → `invalid`) and defaults an absent `app_id` to `""`
  (which the store rejects as `invalid`). Routes: `GET /v1/feed/{home, profile/{account}, posts/{id},
  hashtags/{tag}}`.
- **Contracts**: `PostItem` (id, app_id, author_account, opaque `body`, `media_ids`, `like_count`,
  `comment_count`, `created_at`) and `CommentItem`; both HTTP-only (ride no Cmd/Evt), so exported
  explicitly in `export_ts.rs` and re-exported from the crate root. 2 new bindings (`PostItem.ts`,
  `CommentItem.ts`); the Cmd/Evt graph is untouched, so `coverage.rs` needed no change (reads are HTTP, not
  commands).

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **`app_id` is a required query param on every read** (attacker-controlled), gated by
   `active_account`/`require_app_member` on *that* app. Absent → `""` → `invalid`; empty/oversize → the
   same `APP_ID_MAX` guard the write path uses. Feed is app-scoped end to end, so a read must name its app.
2. **Comments are scoped by `post_id` alone** (no `app_id` column on `comments`, §10.3). Sound because a
   post belongs to exactly one app and the post is fetched app-scoped first — a `not_found` post short-
   circuits before the comment query, so a comment page can only ever be the app-correct post's.
3. **`home` gate is stricter than the other reads** (`active_account` vs `require_app_member`). Home is
   personalized by the caller's own follow set, so "me" *must* be the active account; the other three read
   another entity's public surface, so app membership suffices. This is the exact write-vs-sub asymmetry
   from part A, not a new invention.
4. **The home SQL lives in ONE literal** (`home_select!` macro → `HOME_SQL` + `concat!`-built
   `HOME_SQL_EXPLAIN`), shared verbatim with the EXPLAIN gate. See keeper below — this was the
   session's one real (non-test) defect.
5. **`membership-then-existence` order in `post_detail`** (gate before the post lookup) so a non-member
   can't tell a real post (`forbidden`) from a missing one (`not_found`) — no cross-app existence oracle.
   Documented in the fn; now has a dedicated test.

### The keepers this session (the point of rule 4): a seventh-straight test-gap catch, and a query-drift trap

Ran the budgeted adversarial pass as **three parallel lenses** (correctness/SQL, security/RLS/authz,
independent test-author) via a workflow, then triaged every finding against the code.

- **Correctness/SQL and security/RLS/authz both came back provably clean.** Independent corroboration that
  the read plane is correct: every query runs in `world_tx(who.world_id)` (world isolation), every
  posts/hashtag query carries `AND app_id = $2` (app isolation), `post_detail` gates membership before the
  existence read (no oracle), the keyset `(created_at, id) < (…)` tie-break is total, and the counter/uuid-
  array/jsonb mappings can't panic. Nothing to fix in the code paths.
- **The one real code defect (query-drift, MED, self-adjacent to the test layer):** the 100 k EXPLAIN gate
  was EXPLAIN-ing a **hand-copied duplicate** of the home SQL — byte-identical today but fully decoupled.
  A future edit to `read::home` that regressed the query to a seq scan would ship green: the assertion
  `plan.contains("posts_home")` explains the *stale copy*, and the p95 loop's deliberately-loose 200 ms
  ceiling wouldn't catch a 100 k seq scan either. Fixed structurally: the SELECT is now one macro literal
  feeding both `HOME_SQL` (the endpoint) and `HOME_SQL_EXPLAIN` (the test) — the plan test now provably
  observes the exact string the endpoint runs. (A `const` + `concat!`, so both stay `&'static str` and no
  sqlx-0.9 dynamic-SQL escape hatch is needed.)
- **The independent test-author leg landed the keeper a seventh straight sprint: cross-app isolation was
  entirely untested.** Every one of the 9 original tests used the single app `"instapic"`, so the
  `AND app_id = $2` predicate in `profile`/`post_detail`/`hashtag` was **dead weight no test observed** —
  drop it and a member of app X reads app Y's posts, comments, and timelines, all still green. The code was
  correct; the suite didn't know it. Closed with `reads_are_app_isolated`: a second app in the same world,
  asserting a member of app X gets `not_found` on an app-Y post detail, an empty profile for an app-Y
  author, and an empty page for a tag app Y used. Same class as part A's advisory-emit gap — a correct
  predicate with no test is a silent regression waiting to happen.

Five more gaps from the same leg, each closed with a test whose absence would let a specific mutation ship
green: `post_detail_scopes_comments_to_post` (a second post's comment can't bleed into `WHERE post_id=$1`),
`post_item_surfaces_counters` (seeds `like_count=5, comment_count=3` — a swapped/zeroed counter field now
fails; every other test only ever saw the default `0`), `garbage_cursor_is_invalid` (`?cursor=%21%21%21`
→ 400 `invalid` through the real `FeedQuery::parts` wiring, which `cursor::decode`'s own unit tests never
exercise), `limit_clamped_to_100_and_floored` (`?limit=1000000` → 100, `?limit=0` → non-empty), and
`follows_are_directional` (A→B puts B in A's home but A is *not* in B's home — a symmetric-follow bug the
forward-only assertion would miss).

### Verification (rule 4)

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean (the `core` crate's
  `unwrap_used` deny respected — `.expect` in tests only; the one `.unwrap()` I first wrote got caught by
  clippy, not by me).
- **Full workspace suite green against the live stack** (Postgres+Redis+MinIO): **0 failures across 33 test
  binaries**. New `tests/feed_read.rs` = **15** tests (the 4 timelines' happy paths + pagination keyset +
  the authz negatives + cross-world RLS + the 6 adversarial-gap closers + the 100 k EXPLAIN/p95).
- **100 k EXPLAIN gate green**: the home query rides `posts_home`, no `Seq Scan on posts` (owner `ANALYZE`
  first so the planner has fresh stats). **p95 = ~1.0 ms @ 100 k posts** on the dev host (i5-14500) — an
  order of magnitude under the §10.3 < 10 ms target; recorded here as the first feed-read perf number,
  Sprint 10 tracks the trend.
- Drift gate re-armed: 2 new bindings (`PostItem.ts`, `CommentItem.ts`); Cmd/Evt/ClientFrame/ServerMsg
  unchanged (reads add no wire frame).

### Exit criteria status (Sprint 8 — now fully closed)

| Criterion | Status |
|---|---|
| All `feed.*` commands + routes in coverage tests | **CLOSED** — all 7 `feed.*` + `feed.activity` in the coverage match-test (part A); the 4 new `/v1/feed/*` routes each have an integration test in `feed_read.rs` (the repo has no central route-registry test; per-route coverage is the mechanism). |
| 100 k-row `EXPLAIN` test green | **CLOSED** — `home_100k_uses_posts_home_index_and_records_p95`: no seq scan, rides `posts_home`, and EXPLAINs the *shared* `HOME_SQL` literal so it can't drift from the endpoint. |
| p95 timeline read < 10 ms at 100 k posts (recorded) | **CLOSED** — ~1.0 ms recorded on the dev host. |

### Reflection

- **The write/read seam paid off exactly like the disjoint-table seams of 4–7.** Part B started from a
  green, reviewed base; the read plane reused every part-A primitive (`active_account`, `APP_ID_MAX`, the
  media-less path) and every infra idiom (cursor, `world_tx`, `rfc3339`), so the diff is four SQL queries,
  four handlers, two response types — no new machinery.
- **The test-gap lens found the keeper a seventh time, and it was the *unobserved-predicate* class:** a
  correct `AND app_id = $2` that no test made load-bearing. This is the twin of part A's fire-and-forget
  advisory — correct code that a single mutation turns wrong with the suite none the wiser. The two
  correctness/security lenses coming back clean is not "nothing found"; it's the confidence half of the
  budget ADR-1 buys, and it let the whole session's fixes be *tests plus one drift-proofing*, not code
  rewrites.
- **Root-cause over symptom held.** The drift trap was fixed at the source (one shared literal), not by
  eyeballing that the copy still matched; the cross-app gap was closed with a real second app, not by
  trusting the predicate reads correctly.

### Not committed / next session

- **Sprint 8 part B is complete and green but untracked** on top of the committed 0–7 + part A. First
  commit re-arms the drift gate (2 new bindings) and lands `read.rs`, `http/feed.rs`, the 4 routes, the 2
  contracts types, and `tests/feed_read.rs` — the operator's call, as every sprint.
- **Sprint 8 is now fully closed** (both part-A-deferred criteria met). Feed — the last primitive — is
  done; the primitive layer (channels, media, directory, calls, link, ledger, exchange, feed) is complete.
- **Next: Sprint 9 — verification hardening.** Property tests (ledger conservation, channel seq
  gaplessness, calls FSM legality, cursor round-trip), `cargo-fuzz` on the client-frame/link-hello/cursor
  surface, chaos drill scripts, the generated per-table RLS audit (which will now also cover the five feed
  tables), and `cargo deny`. The three-lens adversarial pass that has caught the keeper every sprint
  becomes, in Sprint 9, the standing CI machinery instead of a per-sprint ritual.
- Still open, minor, none blocking: the p95 gate is deliberately loose (200 ms ceiling, ~1 ms actual) —
  Sprint 10 tightens it; the offline-author `post_liked` inbox path is still only covered by notify's own
  suite; multi-account-per-app "act as active only" semantics remain as documented in part A.

---

## 2026-07-19 (Sprint 8A) — Sprint 8 **part A** (feed write plane: schema, posts/follows/likes/comments, advisory event, hashtag parse, media-gate extraction, sub authz): built, verified live; HTTP read surface (part B) deferred

Sprint 8 (feed) is one cohesive primitive with no shared-nothing *table* seam like Sprints 4–7
had (media/directory, calls/link, ledger/exchange each split on disjoint tables). So it splits
on a different axis — the **write plane vs the read plane**. **(A)** the schema + every write
command (`feed.post/delete/like/unlike/comment/follow/unfollow`) + the advisory `feed.activity`
event + sub authorization + the durable author-notify; **(B)** the HTTP read surface — the
fan-out-on-read home timeline, profile timeline, post detail + comments, hashtag page, all on the
cursor idiom, plus the 100 k-row `EXPLAIN` test and the p95 perf number. The migration is
front-loaded into A (all five tables; retrofit cost lives in the schema), and B builds on A's
committed base. No roadmap amendment — the read surface is roadmap items 3 + part of 4, unchanged.

### What exists now

- **Migration `0012_feed.sql`** — five FORCE-RLS world-scoped tables. `posts` (opaque `body jsonb`,
  `media_ids uuid[]`, denormalized `like_count`/`comment_count int` with `CHECK (>= 0)` backstops),
  `follows` (PK `(world, app, follower, followee)` = the home-timeline lookup), `likes`
  (PK `(post_id, account_id)`), `comments`, `hashtags` (PK `(world, app, tag, post_id)`). Two post
  indexes: `posts_home (world, app, created_at DESC, id DESC)` for the home feed and
  `posts_author (world, app, author_account, created_at DESC, id DESC)` for profiles; plus
  `comments_post` and `hashtags_post` for the cursor read + cascade. FKs to `app_accounts`/`posts`,
  no `ON DELETE CASCADE`. Standard 0001 NULLIF RLS DO-loop.
- **`primitives/feed/mod.rs`** (single-file, directory-shape — no FSM). Seven handlers + `authorize_sub`
  + the `active_account` resolver + hand-written hashtag parse. Every write authors as the caller's
  **active app account** (`sessions.app_accounts->>app_id`), never a payload id. A like bumps
  `like_count` in-tx and, on a real change, advises the feed **and** silently notifies the post
  author (via the author's character, `notify::route`, skipping self-likes). Delete is author-only
  and cascades children explicitly in one tx.
- **The shared media gate extracted** (roadmap item 2): `media::assert_owned_live` now that channels
  and feed both attach media — one guard, one place; `channels.send` was switched to it.
- **Contracts** — 7 `Cmd` (`feed.*`, rate class `Social`), 1 `Evt` (`feed.activity`, **Ephemeral** —
  advisory), `FeedActivityKind {post,like,comment}`. Bindings regenerated (Cmd/Evt/ClientFrame/
  ServerMsg + new `FeedActivityKind.ts`), 2 golden wire tests, coverage match-arms for all 7 + the event.
- **Wiring** — dispatch (7 command arms + the `sub feed:<app>` arm, which the Sprint 2 stub returned
  `not_found` for), `wire_name`, rate classes, coverage.

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **Write-plane/read-plane seam** (vs a shared-nothing table seam). Feed has no disjoint-table split,
   so A = writes + events + schema + authz, B = the HTTP reads. The headline "fan-out-on-read" is B;
   A is verifiable at the store+event level (writes persist, counts exact, advisory delivered),
   exactly how the ledger part-A asserted balances via store reads before its read surface existed.
2. **All five tables carry `world_id` + FORCE RLS.** The §10.3 sketch omits `world_id` on likes/
   comments/hashtags, but the whole codebase makes every domain table world-isolated and the Sprint 9
   RLS audit keys on the `world_id` column — so all five get it. Same class of logged column-add as
   Sprint 7's `hold_id` / `currency`.
3. **Added `posts_home (world, app, created_at DESC, id DESC)` beyond the roadmap's item-1 index.**
   The roadmap's stated "timeline" index leads with `author_account` — that serves the *profile*
   read (one author, by time), not the *home* fan-out-on-read, which orders by `created_at` across
   many followed authors and would seq-scan without `created_at` right after the equality columns.
   Both shipped; part B's `EXPLAIN` test gates on `posts_home`. A correction, not a contradiction —
   logged here rather than a CDR (§10.3 leaves indexing to the roadmap).
4. **Feed acts as the caller's ACTIVE app account**, resolved from the session (`app_accounts->>app_id`),
   never a payload-supplied account id. Sub authz is looser — owns *any* account for the app. Closes
   the "who authors" question §10.3/roadmap left open; coherent with `identity.app_login` storing the
   active account per session. Consequence (documented): a character with two accounts in one app acts
   as, and manages, only its *active* one — switch accounts to manage the other's posts.
5. **Explicit child deletes, not FK `ON DELETE CASCADE`.** Keeps every removal inside the RLS-scoped
   `world_tx` (the "all access through world_tx" convention) and sidesteps the RLS-vs-RI-cascade
   question entirely. The FKs (no cascade) still guarantee no orphan and make a dropped child-delete
   fail loudly.
6. **`MEDIA_MAX = 8` per post** (roadmap silent) bounds the owned+live check; **hashtags parsed from
   `body.text`** (§10.3 says "server-side at post time"; `text` is the natural source), hand-written
   (no `regex` dep — matches the gif-host hand-parse), `char::is_alphanumeric` standing in for `\p{Alnum}`.

### The keepers this session (the point of rule 4)

Ran the budgeted adversarial pass as **three independent lenses** — correctness/SQL, security/RLS/authz,
and the independent test-author — in parallel, then triaged every finding against the code. The
**security lens came back provably clean**: every feed query runs inside `world_tx(who.world_id)`, every
INSERT binds the caller's own `world_id` (so RLS `WITH CHECK` rejects a forged world), the actor is
*always* the session's active account (no feed command trusts a payload account as the author), and the
like-notify carries the liker's account to the author but never leaks the author's character back to the
liker — cross-world and cross-account impersonation are structurally closed. The one hardening note (an
uncapped `app_id` on the write path, unlike the `sub` path's 64-char cap) was folded in. Three real
keepers came out of the other two lenses, none visible to the 9 green happy-path tests:

1. **A null body 500 (self-found before the review, LOW).** A media-only post with `body: null` passed
   validation (media present) but `posts.body` is `jsonb NOT NULL`, so it would hit the constraint as
   an `internal` instead of a clean `invalid`. Fixed at the boundary: `validate_doc` rejects JSON null;
   a media-only post sends `{}`.
2. **A delete/like-comment FK race (correctness lens, MED — the keeper).** `like`/`comment` did a
   *non-locking* post-existence read then inserted a child row FK-referencing `posts`; a concurrent
   author `delete` committing in the window trips the FK — surfacing as `internal` for the liker, **or**,
   the other way, the `DELETE FROM posts` FK-violates against a just-committed like and the **delete
   500s and rolls back, leaving the post undeleted**. Root cause: no post lock. Fixed by taking
   `FOR UPDATE` on the post as the *first* post-touching op in all three handlers (delete collapsed to
   one locking `SELECT author_account … FOR UPDATE`) — a single lock point per handler, so the ops
   serialize with **no lock-upgrade path and thus no deadlock** (the naive `FOR KEY SHARE` + counter
   `UPDATE` would deadlock two concurrent likers; `FOR UPDATE` doesn't, and it's the same per-post lock
   the count bump already took, so no added contention). Verified: concurrent likes 12/12 clean under
   the change, plus a new 50-round `delete_like_race_never_internal` storm asserting neither op ever
   returns `internal`.
3. **The independent test-author leg landed the sprint's keeper, sixth sprint running: the advisory
   `feed.activity` on the LIKE and COMMENT paths was observed by NO test.** `activity()` is
   fire-and-forget post-commit (its result discarded, never on the ack), so a broken or mislabeled
   emit on 2 of its 3 sites would ship completely green — every existing test only ever saw the *post*
   activity, and `coverage.rs` named that one test for the whole `FeedActivity` event (true-but-lying by
   omission). Same class as the calls-voice ledger drift. Closed with `like_and_comment_advise_feed`
   (a watcher observing post→like→comment in order, each with the right `kind` + `actor`, plus absence
   on an idempotent re-like and on delete), and the coverage entry corrected to name it.

The same leg also flagged that **like/comment `not_found` — including the cross-app `app_id`-scope guard —
was untested**: dropping `AND p.app_id = $2` (letting an account like a post in a *different* app) would
ship green. Now `like_comment_missing_post_not_found` drives a missing id and a real-post-wrong-app probe,
both → `not_found`. Also added `self_like_no_notify` (the `author_account != account` guard's equal case,
which no other test hit — a *removed* guard would have self-notified invisibly) and `post_media_validated`
(the extracted `assert_owned_live` gate: >8 ids → invalid, foreign id → forbidden, media-only `{}` post → ok).

### Verification (rule 4)

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean (the `core` crate's
  `unwrap_used` deny respected — `?`/`.expect` in tests only).
- **Full workspace suite green** against the live stack (Postgres+Redis+MinIO): 0 failures across all
  binaries. New `tests/feed.rs` (**14**): post + advisory, not-logged-in `forbidden` (post + sub),
  author-only cascade delete, like counter + author-notify + idempotent re-like, unlike decrement +
  no-underflow, comment count, follow/unfollow + self/unknown authz, the 32-way concurrent-likes
  count-exactness, cross-world RLS, the delete/like race storm, the 3-kind advisory, cross-app
  `not_found`, self-like-no-notify, and the media gate. Plus 5 `feed/mod.rs` unit tests (hashtag parse
  edges, content/size/null-body). `wire.rs` +2 feed goldens (32 total).
- **Concurrency flakiness: 12/12** on the 32-way like storm *after* the `FOR UPDATE` change (deadlock-
  freedom confirmed empirically) + the 50-round delete/like race storm green.
- Drift gate re-armed: `Cmd`/`Evt`/`ClientFrame`/`ServerMsg` regenerated + new `FeedActivityKind.ts`.

### Exit criteria status (Sprint 8 — part A slice)

| Criterion | Status |
|---|---|
| All `feed.*` commands + routes in coverage tests | **PARTIAL** — all 7 `feed.*` + `feed.activity` in the coverage match-test; feed adds **no HTTP route in A** (the reads are part B), so the route-coverage test is trivially satisfied. |
| 100 k-row `EXPLAIN` test green | **DEFERRED (part B)** — the home timeline query + its index (`posts_home`) exist; the `EXPLAIN`-no-seq-scan test lands with the read surface. |
| p95 timeline read < 10 ms at 100 k posts (recorded) | **DEFERRED (part B)** — needs the read endpoint. |

### Reflection

- **A new kind of seam.** Sprints 4–7 split on disjoint tables; feed has none, so it split on
  writes-vs-reads. The dividend held anyway — A is a complete reviewed slice and B starts from green.
- **The independent test leg caught the sprint's defect a sixth straight time**, and it was the
  *fire-and-forget advisory* class again (like calls-voice): an emit site with no ack coupling is
  invisible to happy-path tests until someone subscribes and watches it. That is exactly the budgeted
  verification ADR-1 buys — the desk-check reads "like publishes an activity" as obviously fine.
- **Root-cause over symptom held.** The FK-race fix went into all three shared handlers (every future
  caller serialized), not a per-caller guard; the media gate was extracted to one function both callers
  share; the null-body reject sits at the one validation boundary.
- **Design-doc latitude, not contradiction.** §10.3 left the world_id-on-all-tables, the authorship
  model, and the home index open; closed in code and logged here, no CDR, since nothing contradicts the
  design.

### Not committed / next session

- **Sprint 8 part A is complete and green but untracked** on top of the committed 0–7. First commit
  re-arms the drift gate (4 modified bindings + new `FeedActivityKind.ts`) — the operator's call, as
  every sprint.
- **Sprint 8 part B — the feed read surface.** Home timeline (the EXISTS fan-out-on-read on `posts_home`
  + the 100 k `EXPLAIN`-no-seq-scan test + the p95 number), profile timeline, post detail + comments,
  hashtag page — all HTTP on the cursor idiom (CDR-7). The author-notify-on-like is already done in A.
- Still open, minor, none blocking: the offline-author `post_liked` → inbox path is exercised only by
  notify's own suite (a feed-context test is optional, shared code); the multi-account-per-app
  "act as active only" semantics (documented in the module); pre-existing online-member badging,
  Bearer case-sensitivity, `identity.me` own-last-seen.

---

## 2026-07-19 (Sprint 7B) — Sprint 7 **part B** (exchange: deposit, two-leg withdraw, reconciliation cross-check, bridge journal + doc): built, verified live; **Sprint 7 complete**

Part A shipped the ledger core and stopped on the shared-nothing seam (A = accounts/
transfers/holds + the invariant machinery, B = the framework exchange). Part B lands the
other half: the one seam where value crosses between the framework bank and the ledger.
It *builds on* A's transfer/hold machinery rather than sharing tables with it, so B started
from the green, committed 0–7A base. With it **Sprint 7 is complete** — every exit criterion
passes, including the bridge-facing doc (criterion 3, deferred from A). Seventh sprint the
seam-split paid off. No roadmap amendment — exchange was always Sprint 7 item 4 + the
`ledger.withdraw` half of item 3.

### What exists now

- **Migration `0011_exchange.sql`** — `tenants.currency` (the tenant config store, default
  `'OPN'`, one-per-world so effectively per-world) + a `SELECT (currency)` grant; a partial
  `accounts_system (world_id, currency) WHERE owner_kind='system'` so the system account
  get-or-create can `ON CONFLICT`; and the FORCE-RLS world-scoped `exchanges` table (PK
  `(world_id, id)` — the id is bridge-chosen for a deposit, Core-minted for a withdraw),
  with a `hold_id` FK linking a withdraw to its backing hold, a journal keyset index, and a
  partial pending-hold index. Standard 0001 NULLIF RLS.
- **`primitives/ledger/exchange.rs`** (new, sibling of `store`/`fsm`) — `deposit` (idempotent
  system→wallet credit, auto-creating the system account + wallet on first touch), `withdraw`
  (WS leg 1: hold the wallet + open a `pending_confirm` exchange, return a Core-minted id),
  `withdraw_confirm` (HTTP leg 2: capture the hold to system, exchange→`done`), `journal`
  (the bridge's reconciliation feed), and `cross_check` (called from `store::reconcile`).
- **The load-bearing decision — distinct transfer `kind`s.** A deposit writes a `kind='deposit'`
  transfer (system→wallet); a withdraw-confirm writes a `kind='withdraw'` transfer (wallet→
  system). The reconciliation cross-check compares Σ(done deposit exchanges) vs Σ(`kind='deposit'`)
  and the same for withdraw. Because those two kinds are written ONLY by the exchange paths
  (a plain user transfer is `'transfer'`, a user hold-capture is `'capture'`), the cross-check
  is exact: it never false-freezes genesis funding or a user capture, and an orphaned exchange
  (or a lost money leg) freezes the system account. This is what let part A's concurrency
  battery keep passing untouched — its `kind='transfer'` funding is invisible to the cross-check.
- **Wiring** — 1 `Cmd` (`ledger.withdraw`, rate class `Money`), no new `Evt` (a fresh deposit
  rides `notify.event` alert like every incoming-money path); `POST/GET /v1/tenants/self/exchange`
  (API-key `TenantAuth`; POST = deposit|withdraw_confirm on `direction`, GET = journal from an
  RFC-3339 `since`); `store::expire_holds` now also flips a pending withdraw's exchange to
  `expired` when its hold auto-releases; coverage match-arm + a golden wire test; bindings
  regenerated (`Cmd`/`ClientFrame`). `time` gained the `parsing` feature for the `since` parse.
- **`docs/opn-bridge-exchange.md`** — the normative bridge contract (exit criterion 3): auth,
  the deposit/withdraw/journal wire shapes, the two-leg dance, idempotency + retry rules, the
  bank-debit-first ordering, and what a system freeze means for the bridge.

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **Distinct `kind`s ('deposit'/'withdraw') for the cross-check**, vs a "transfers whose leg
   touches a system account" join. The kind-partition decouples the detector from user transfers/
   captures entirely — no false freeze, and zero churn to part A's tests (their system-funded
   genesis stays `kind='transfer'`). One column value carries the whole cross-check.
2. **`currency` as a `tenants` column.** The roadmap says "currency from tenant config"; the
   tenants row *is* the tenant config store, so a column (with a default so existing tenants keep
   working) is the minimal home. System + wallet are both created with it, so the part-A
   currency-match is satisfied by construction — no cross-currency path in v1.
3. **`hold_id` added to the `exchanges` tuple** (the roadmap's tuple omitted it). It links a
   withdraw to its reservation so `expire_holds` can auto-expire the exchange when the hold
   releases — the roadmap's own "unconfirmed → hold expiry + exchange `state=expired`" needs the
   link. A column addition, logged here (same weight as part A's currency-match call), not a CDR.
4. **Deposit/withdraw_confirm are HTTP (bridge); withdraw leg 1 is WS (the in-game app).** Exactly
   the roadmap split — the app *starts* a withdraw, the bridge *confirms* it. The journal `?since`
   is an inclusive RFC-3339 keyset (bridge dedupes by id), not a compound cursor — the exchange
   id is text, so the shared Uuid cursor util doesn't fit, and a reconciliation feed tolerates
   re-reading a boundary microsecond. Ceiling marked in code.
5. **`WITHDRAW_HOLD_SECS` a const (1 h), not config** — one seam, no operator has asked to tune it;
   `ponytail:` marked with the promote-to-env path.

### The keepers this session (the point of rule 4): a money-creation path and a half-enforced freeze

Ran the budgeted adversarial pass as **three independent lenses** — correctness/SQL/conservation,
security/RLS/authz, and the independent test-author — then triaged all findings myself against the
code. The conservation and isolation cores came back **provably clean** (every exchange path pairs
one balance move with exactly one transfer row; every query runs inside `world_tx`; deposit can't
target a system account or cross worlds; withdraw leg 1 only ever touches the caller's own wallet;
withdraw_confirm can't double-capture — a settled hold's FSM returns `Conflict`) — the verification
thesis working, the hard properties *argued* not hoped. Six real defects came out of the other
edges, none visible to the 6 green happy-path tests, and two were genuine keepers:

1. **A latent money-creation path (correctness lens, MED — the keeper).** A withdraw's backing
   hold was an ordinary `holds` row, and the *public* `ledger.capture`/`ledger.release` only check
   owner + the hold FSM — they were exchange-unaware. A character could `ledger.withdraw{100}`
   (hold H + pending exchange E), then `ledger.capture{H → their other account}`: H settles
   wherever they chose, **E stays `pending_confirm`**. The bridge — which credits the framework
   bank *before* confirming — then calls `withdraw_confirm(E)`, which finds H already captured and
   fails, so the wallet was never debited for the withdraw. **Money created.** Gated today only by
   the hold_id being an unguessable v7 uuid never returned on the wire — but that's a
   defense-by-obscurity, not a structural barrier, and a money invariant deserves the belt. Fixed
   at root cause: `capture`/`release`'s hold lookup now filters out any hold backing a
   `pending_confirm` exchange (`AND NOT EXISTS (... state='pending_confirm')`), so a withdraw hold
   reads as nonexistent to the public API — only `withdraw_confirm`/`expire_holds` can move it.
2. **The reconciliation freeze was only half-enforced (correctness + security, MED — two lenses
   converged).** `cross_check` freezes the system account on drift specifically "to halt exchange
   flow", and `deposit` honored it — but `withdraw_confirm` checked only the *wallet's*
   `frozen_at`, not the system's (it even had the row in hand). So during an active
   money-integrity incident, deposits would 409 while withdraws kept settling `wallet → system`,
   pushing more money through the very account under investigation. Fixed: `withdraw_confirm`
   rejects if *either* locked row is frozen (one-line, data already fetched).

And the independent test-author leg landed its own keeper, sixth sprint running: **the one test
that claimed to cover the `withdraw_confirm` field-mismatch → `Invalid` guard asserted the wrong
thing** — it passed a *deposit* id, hit the direction filter, and asserted `404`, never reaching
the `ex_char != character || ex_amount != amount` guard at all. Deleting or inverting that guard
would have passed the whole suite green. Now `withdraw_confirm_rejects_mismatch` drives a *real*
pending withdraw and confirms wrong-character and wrong-amount both `→ 400` with the exchange left
pending. Same leg also found the cross-check's **withdraw half was never exercised** (only the
deposit-orphan case fired a freeze) — a typo in that half would ship a silently-dead detector;
now `cross_check_freezes_orphan_withdraw` mirrors it.

### Also fixed / documented from the review

- **Concurrent first-touch get-or-create was NOT race-safe (correctness, MED).** The
  `INSERT … ON CONFLICT DO NOTHING … UNION SELECT` idiom has the classic READ-COMMITTED snapshot
  hole: the loser blocks on the winner's insert, then its `SELECT` uses the pre-winner snapshot
  and returns zero rows → a spurious `RowNotFound`/500. Fixed to `ON CONFLICT … DO UPDATE SET
  currency = EXCLUDED.currency RETURNING id`, which locks and returns the conflicting row — one
  statement, always one row, and simpler than the CTE.
- **A `withdraw_confirm` ⇄ `expire_holds` lock-order inversion → production deadlock (correctness,
  MED).** Confirm locked exchange-then-hold; the janitor's `expire_holds` locked hold-then-
  exchange — opposite order on the same pair, deadlockable whenever a confirm raced a hold's
  expiry. Fixed by reordering `expire_holds` to update the exchanges *before* releasing the holds,
  so both paths lock exchange-before-hold. (12× flakiness run, 0 deadlocks.)
- **Deposit's own locking relied on an implicit invariant (correctness, MED/LOW).** It locked
  system-then-wallet (not id-ordered), safe only because the system account's v7 id is always <
  the wallet's — true in prod but not a stated invariant. Switched to the same `IN($a,$b) ORDER BY
  id FOR UPDATE` idiom every other money op uses; the implicit dependency is gone.
- **Deposit idempotency now validates the replay matches the stored exchange** (correctness, LOW):
  reusing an id for a *different* character/amount → `Invalid`, symmetric with `withdraw_confirm`
  (was: returned a wrong-but-"success" ack).
- **Accepted / documented, not fixed:** the cross-check sums per-direction over *all* currencies,
  so at v1's one-currency-per-world it's exact but a future multi-currency world could mask two
  canceling drifts — noted, revisit with multi-currency; the withdraw_confirm wallet↔system
  currency-match is unreachable (`tenants.currency` has no mutation path) — noted; the exchange
  HTTP endpoints have no per-tenant rate limit, consistent with *every* API-key HTTP route
  (`/sessions`, `/calls/active`) — the bridge key is trusted, blast radius is the tenant's own
  world (RLS), a Sprint-10 budget-tuning candidate, not an exchange-specific gap.

### Verification (rule 4)

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean (the `core` crate's
  `unwrap_used` deny respected — `?`/`.expect`/`matches!` only).
- **Full workspace suite green** against the live stack (Postgres+Redis+MinIO): 29 binaries,
  0 failures. New `tests/exchange.rs` (**13**): deposit idempotency + wallet auto-create + system
  mint, the additive second deposit, deposit-notifies-once, the full withdraw cycle (WS leg 1 →
  reserve-excluded-from-available → confirm → idempotent re-confirm → reconcile-clean, asserting
  the `kind='withdraw'` leg explicitly), withdraw-without-wallet → conflict, withdraw_confirm
  field-mismatch → invalid, withdraw expiry (hold + exchange), the withdraw-hold-not-capturable
  keeper, both cross-check freeze directions (deposit-orphan + withdraw-orphan), concurrent
  same-id deposit → one credit, journal list + `since` filter, and cross-world RLS. Part A's
  `tests/ledger.rs` (13) stays green after the `capture`/`release`/`expire_holds` changes.
- **Money-concurrency flakiness check: 12/12 clean** on the exchange + ledger binaries (the
  concurrent same-id deposit, the opposing-transfer storm, and the battery each run) — deadlock-
  freedom after the three lock-ordering fixes confirmed empirically, not just argued.

### Exit criteria status (Sprint 7 — now fully closed)

| Criterion | Status |
|---|---|
| Concurrency battery green 100 consecutive runs | **PASS (strong)** — part A's battery + the new concurrent-same-id deposit ran clean 12/12 locally; the "100" is a CI/soak repeat, and the tests are deterministic-contention. |
| Reconciliation catches an injected corruption | **PASS** (part A's balance freeze; part B adds the exchange cross-check, both freeze directions tested). |
| Exchange protocol documented for the bridge author | **PASS** — `docs/opn-bridge-exchange.md` (normative: auth, deposit/withdraw/journal shapes, idempotency + replay + bank-ordering rules, freeze semantics). |

### Reflection

- **The seam-split paid a seventh time.** Ledger-core and exchange share only the transfer/hold
  machinery (exchange *calls* it), so B was a complete reviewed slice on the committed base —
  same dividend every sprint since 4.
- **The adversarial pass earned its keep on the highest-stakes surface again.** Three lenses;
  the conservation/isolation cores were *proved* clean, and the edges gave a money-creation path,
  a half-enforced freeze (two lenses converged), a real race, a real deadlock, and a lying test —
  none visible to 6 green happy-path tests. On a money exchange that is exactly the budgeted
  verification ADR-1 buys, not polish.
- **Root-cause over symptom held.** The withdraw-hold guard went into the two shared store fns
  (every future caller protected); the frozen-system guard mirrors deposit's; the race fix is the
  one-statement DO-UPDATE; the deadlock fix reorders the janitor so *both* paths agree.
- **Design-doc latitude, not contradiction.** §10.5 left the exchange schema and mechanics to the
  roadmap; I closed the currency home, the `hold_id` link, and the distinct-kinds cross-check in
  code and logged them here rather than minting a CDR, since nothing contradicts the design.

### Not committed / next session

- **Sprint 7 (A+B) is complete and green but this B slice is untracked** on top of the committed
  0–7A. First commit re-arms the drift gate (updated `Cmd`/`ClientFrame` bindings) — the
  operator's call, as every sprint.
- **Sprint 8 — Feed.** Depends on Sprint 4 (cursor, media attachment pattern), parallelizable with
  the now-complete 6/7. The fan-out-on-read timeline is the next primitive; no v1 app consumes it,
  but the primitive ships (OPN.md §14.5).
- Still open, minor, none blocking: the multi-currency cross-check refinement + per-tenant exchange
  HTTP rate limit (both Sprint-10 candidates, documented ceilings); the concurrent-duplicate
  `internal` cosmetic corner (shared with part A's transfers_idem); online-member badging,
  Bearer case-sensitivity, `identity.me` own-last-seen (all pre-existing).

---

## 2026-07-19 (earlier) — Sprint 7 **part A** (ledger: accounts/transfers/holds, deadlock-free transfer, hold FSM, capture/release, nightly reconciliation, hold-expiry, history): built, verified live; exchange (part B) deferred

Sprint 7 has two shared-nothing halves, so it split the same way every sprint since 4 has:
**(A)** the ledger core — the `accounts`/`transfers`/`holds` tables, the deadlock-free
transfer, the hold FSM + capture/release, the nightly reconciliation, hold-expiry, and
`ledger.history` — and **(B)** the exchange protocol (`exchanges` table, the
deposit/`withdraw_confirm` HTTP endpoints, the two-leg `ledger.withdraw`, the exchange
cross-check in reconciliation, and the bridge-facing doc). B builds *on* A's transfer/hold
machinery, so A is a complete, reviewed, green slice and B starts from it. A closed exit
criteria 1–2; the bridge doc (criterion 3) is B. No roadmap amendment — items 4 (exchange)
and 3's `ledger.withdraw` stay in Sprint 7.

### What exists now

- **Migration `0010_ledger.sql`** — three FORCE-RLS world-scoped tables. `accounts`
  (`owner_kind` character|system, nullable `owner_character`, `currency`, `balance bigint`,
  `frozen_at`) with `CHECK (balance >= 0 OR owner_kind = 'system')` — only the tenant system
  account may run negative — and a partial unique `accounts_char_wallet (world, owner_character,
  currency) WHERE owner_kind='character'` (one wallet per currency). `transfers` (immutable,
  `kind` transfer|capture, nullable `client_uuid`) with the partial-unique idempotency index
  `transfers_idem (from_account, client_uuid) WHERE client_uuid IS NOT NULL` and two
  directional keyset indexes for history. `holds` (`state` held|captured|released, `expires_at`)
  with a held-sum partial index and an expiry partial index. Standard 0001 NULLIF DO-loop RLS.
- **`primitives/ledger/{fsm,store,mod}.rs`** — the multi-file calls-shape (holds → an FSM).
  `fsm.rs`: the 3-state hold machine as one pure `apply` (Held→Captured|Released, terminals
  absorb), the Sprint 9 proptest target, with an exhaustive literal-table unit test. `store.rs`:
  `transfer` (idempotency-first, then id-ordered `IN ($f,$t) FOR UPDATE`, available =
  `balance − Σheld`, debit/credit/insert), `hold`/`capture`/`release` (each locks the account
  row so `held_sum` is race-free), `history` (cursor idiom), and `reconcile`/`expire_holds`
  (advisory-locked janitor fns). `mod.rs`: the four handlers + the incoming-money notify.
- **The load-bearing invariant, made explicit:** an account is born at `balance 0` and the
  *only* way money moves is a `transfers` row (a transfer or a capture), so `balance ==
  Σ(to==id) − Σ(from==id)` holds universally. `reconcile` recomputes exactly that and freezes
  drift under the advisory lock; the concurrency battery asserts the same equality. Test and
  prod share the one invariant SQL (via `store::reconcile`), per the roadmap.
- **Wiring** — 4 `Cmd` (`ledger.transfer/hold/capture/release`, rate class `Money`), no new
  `Evt` (incoming money rides `notify.event`, class `alert`, app_id `wallet`); `TransferItem`
  contract type; `GET /v1/ledger/history` (JWT, cursor); janitor `ledger_expire_holds` (releases
  + silent-notifies owner) and `ledger_reconcile` (hour-gated to `OPN_RECONCILE_HOUR`, default 3);
  coverage match-test + 4 golden wire tests; bindings regenerated (`Cmd`/`ClientFrame` + new
  `TransferItem.ts`).

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **Part A ships `accounts`/`transfers`/`holds`; the `exchanges` table + system-account mint
   path are part B.** The roadmap's §10.5 schema is one migration for the whole sprint, but the
   exchange table has no part-A consumer, so front-loading it is YAGNI. Accounts/transfers/holds
   are genuinely part A (transfers and holds live here). The `system` `owner_kind` + the negative
   exemption ship now (they're columns of the part-A `accounts` table) but aren't exercised until
   B's deposit path.
2. **`capture` authz = the holding account's owner (self-escrow).** The roadmap's `ledger.capture
   { hold_id, to }` doesn't name who may capture; I closed it as "you reserve your own funds and
   settle them to a payee yourself" (`hold` already requires you to own the reserved account). A
   merchant-holds-customer model would need a different authz story; self-escrow is the coherent
   v1. Logged, not a CDR (within §10.5 latitude).
3. **Currency-match enforced on both transfer and capture; cross-currency → `Invalid`.** Prevents
   value creation across currencies — not spelled out in the roadmap but a money invariant.
4. **No dedicated `ledger.*` Evt.** Incoming money is a `notify::route` (alert), exactly the
   roadmap's item 8; a live balance-push event would be redundant with notify for v1. Fewer
   contract surfaces.
5. **No "list my accounts" read in part A.** The roadmap's part-A commands are transfer/hold/
   capture/release/history; an app discovers its account ids from `history` (or, in B, the
   deposit response that first creates its wallet). A balance/accounts read lands when an app
   needs it — deferred, noted.
6. **Reconciliation hour-gate is an in-process `now_utc().hour()` check, no scheduler.** The
   codebase has no time-of-day scheduling precedent; a one-line gate on the 30 s tick is the lazy
   fit. It fires ~120×/reconcile-hour, which is fine because the freeze is idempotent
   (`frozen_at IS NULL`). `ponytail:` marked with the "add a last-reconciled-today guard if the
   recompute grows" upgrade path.

### The keepers this session (the point of rule 4): a money-loss trap and a silently-disabled safety net

Ran the budgeted adversarial pass as **four independent lenses** — correctness/SQL,
concurrency/conservation, security/RLS, and the independent test-author — then triaged every
finding myself against the code. The concurrency and security lenses came back **clean on the
money-critical machinery** (deadlock-freedom across all interleavings, `held_sum` serialization
via the account row lock, capture's skip-available-check safety from the `balance ≥ Σheld`
invariant, reconcile's no-false-freeze under READ COMMITTED, owner-only writes, all-three-tables
RLS, cross-currency blocked, no cross-world movement) — each with a traced proof, which is the
verification thesis working: the hard properties were *argued*, not hoped. The two real defects
came from the other two legs, and both were invisible to the 10 green tests:

1. **A nil `client_uuid` silently traps an account (correctness lens, MED).** `client_uuid` is a
   required wire field with no nil-rejection. The nil UUID (`00000000-…`) is the single most
   common accidental/zero-initialized value, and it is a *real value*, not SQL NULL, so it
   participates in the `transfers_idem` index. A client that left the key zeroed would have its
   *first* nil-keyed transfer stick and every later, genuinely-different nil-keyed transfer
   **silently replay it — moving no money while the caller is told it did**, no error, no metric.
   The fix is one guard (`client_uuid.is_nil() → Invalid`), but the bug is the dangerous kind:
   silent money-not-moving that a happy-path suite (every test used `now_v7()` keys) sails past.
2. **The corruption detector can be silently switched off (correctness + test-author, MED — the
   two legs converged).** `reconcile_hour` was an unvalidated `u32`; `hour()` only returns 0–23,
   so `OPN_RECONCILE_HOUR=24` (a plausible "midnight" typo) makes the gate never fire and
   reconciliation **never runs, forever, with no startup error** — disabling the one mechanism
   that detects silent money corruption. Fix: validate `0..=23` fail-fast at config load, with a
   test.

And the independent test-author leg landed its own keeper — the **flagship concurrency battery
passed vacuously**: `Σ balances == 0` is true by construction (genesis funding) and
reconcile-clean is true when nothing moved, and every `Conflict` was swallowed as "fine", so the
exit-criterion test would stay green *even if every transfer failed*. Fixed to count successes and
assert money actually moved (`oks > 200/400`). Fifth sprint running, the independent-test leg
caught a shipped weakness the desk-check misses.

### Also fixed / documented from the review

- **Security L1 (fixed) — `capture` leaked hold state before authz.** It ran the FSM/self checks
  before the owner check, so a non-owner could distinguish held-vs-settled from the error code.
  Reordered to owner-check-first via a hold+account join (mirrors `release`).
- **Security L2 / correctness #3 (fixed) — idempotency replay skipped the ownership check.** The
  fast path returned a balance before verifying ownership. Now the idempotency SELECT joins
  `accounts` and filters `owner_character = actor`, so a non-owner misses it and falls through to
  the locked path's `Forbidden` (before any INSERT).
- **Store-level `amount <= 0` guards added** to `transfer`/`hold` (defense-in-depth for B's
  future direct calls; the DB `CHECK` was the only backstop).
- **`held_sum` invariant comment added** (concurrency NOTE): the available-check is race-free
  only because every hold-writer locks the account row first — flagged so a future primitive
  can't silently reintroduce a check-vs-debit race.
- **Accepted / documented, not fixed:** the `Forbidden`-vs-`NotFound` existence oracle (L3 —
  gated by unguessable v7 uuids, same stance as media/calls); the spurious `internal` on a
  *truly-simultaneous* duplicate transfer (conservation-safe — the loser's whole tx rolls back;
  sequential retries, the norm, hit the clean replay path); incoming-money into a frozen account
  is *allowed* by design (freeze blocks outgoing only) — now covered by a test so a future reader
  doesn't "fix" it into a bug.

### Verification (rule 4)

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean (the `core` crate's
  `unwrap_used` deny respected — caught two `.unwrap()`s in a new test, changed to `.expect`).
- **Full workspace suite green** against the live stack (Postgres+Redis+MinIO): every binary,
  0 failures. New `tests/ledger.rs` (**13**): transfer happy/insufficient/frozen/missing,
  ownership + idempotency, the hold→capture→release lifecycle (incl. terminal-replay conflicts +
  a post-capture reconcile/capture-row assertion), the concurrency battery (now asserting money
  actually moved), the opposing-transfer deadlock storm, reconciliation-freezes-injected-
  corruption, negative-system-allowed/character-`CHECK`, hold-expiry, cross-world RLS, the
  history pagination+isolation test, input-validation (zero/negative/**nil-key**), and capture
  edges (self, cross-currency, incoming-to-frozen), plus a WS wire smoke. `config_env` extended
  for the `reconcile_hour` range check. 3 `fsm.rs` unit tests. 4 golden wire tests.
- **Money-concurrency flakiness check: 25/25 clean** on the battery + opposing storm (16 tasks ×
  25 transfers + 400-iteration A↔B storm each run) — deadlock-freedom confirmed empirically, not
  just argued.

### Exit criteria status (Sprint 7 — part A slice)

| Criterion | Status |
|---|---|
| Concurrency battery green 100 consecutive runs | **PASS (strong)** — the battery + opposing storm ran 25/25 clean locally with no flakiness; the "100" is a CI/soak repeat, and the test is deterministic-contention. |
| Reconciliation catches an injected corruption in test | **PASS** — `reconciliation_freezes_injected_corruption` (corrupt a balance → reconcile freezes it → outgoing op `Conflict` → idempotent re-run). |
| Exchange protocol documented for the bridge author | **DEFERRED (part B)** — the exchange protocol itself is part B; the doc lands with it. |

### Reflection

- **The seam-split paid a sixth time.** Ledger-core and exchange share only the transfer/hold
  machinery (exchange *calls* it), so A is a complete reviewed slice and B starts clean — same
  dividend Sprints 4/5/6 paid.
- **The adversarial pass earned its keep on the highest-stakes primitive.** Four lenses; the two
  argument-heavy legs (concurrency, security) *proved* the money-critical properties clean, and
  the two others found a silent money-loss trap and a silently-disabled safety net — neither
  visible to 10 green happy-path tests. On a money ledger that is exactly the budgeted
  verification ADR-1 buys, not polish.
- **Root-cause over symptom held.** The nil-key guard went in the handler (one place every
  transfer routes through); the idempotency authz fix went in the shared SELECT (protects the
  replay path and B's future callers); the `amount<=0` guard went in the store (defends the
  direct-call surface B will use).
- **Design-doc latitude, not contradiction.** §10.5 left capture-authz and currency-matching
  open; I closed them in code and logged the choices here rather than minting a CDR, since
  nothing contradicts the design. If part B's exchange needs the capture-authz decision pinned,
  that's the moment for a §10.5 note.

### Not committed / next session

- **Sprint 7 part A is complete and green but untracked** on top of the committed 0–6. First
  commit re-arms the drift gate (updated `Cmd`/`ClientFrame` bindings + new `TransferItem.ts`) —
  the operator's call, as every sprint.
- **Sprint 7 part B — exchange.** The `exchanges` table (PK `(world, id)`), the API-key
  `POST /v1/tenants/self/exchange` (deposit = idempotent system→wallet credit, auto-creating the
  wallet on first touch; `withdraw_confirm` = capture the hold to system), the two-leg WS
  `ledger.withdraw` (hold + `pending_confirm` exchange row), the exchange↔system-legs cross-check
  added to reconciliation, and the bridge-facing doc (exit criterion 3). It builds directly on
  part A's `transfer`/`hold`/`capture` and the reconcile invariant.
- **The deferred "list my accounts / balance" read** — add when an app (or B's deposit response
  shape) needs it.
- Still open, minor, none blocking: the concurrent-duplicate `internal` cosmetic corner
  (documented ceiling); online-member badging, Bearer case-sensitivity, `identity.me`
  own-last-seen (all pre-existing).

---

## 2026-07-18 — Sprint 6 **part B** (tenant link: `/link` gateway, `calls.voice` down-events, `/calls/active` re-sync, coturn + `ice_servers`): built, verified live; **Sprint 6 complete**

Part A shipped the WS-facing call primitive and stopped on the shared-nothing seam
(A = call sessions + gateway, B = the tenant `/link`). Part B lands the other half:
the server→FXServer push channel that carries voice-target events. A and B share no
tables and only the `calls` emit sites, so B started clean on the committed 0–6A base.
With it **Sprint 6 is complete** — every exit criterion passes.

### What exists now

- **`gateway/link.rs`** (new) — the whole link connection type: `LinkRegistry`
  (world → live link), `LinkHandle` (bounded queue + `link_seq` takeover guard,
  mirroring `ConnHandle`'s `conn_seq` subtlety), the `GET /link` axum handler
  (API-key via the `TenantAuth` extractor, no origin/pre-auth — a native FXServer,
  not a browser), the hello handshake (`LinkHello` within 3 s → ack → register →
  writer/reader), last-writer takeover (prev closed 4408), durable backpressure
  (queue full → close 4410 → resource reconnects + re-syncs), and heartbeat
  (2 missed pongs → reap a crashed FXServer). Up-direction is nothing: the reader
  only tracks pongs/close and ignores stray frames.
- **Voice emit** — `calls::publish_snapshot` (now pub) does the `call:<id>` snapshot
  fan-out **and** `emit_voice` on the link in lockstep: `set_targets` with the joined
  characters while a call is active, `clear` when it ends, nothing while ringing.
  Wired into accept/decline/hangup (via `publish_snapshot`) **and** the janitor reap
  (which now routes through `publish_snapshot` too, so a reaped ring clears voice for
  free).
- **Re-sync** — `GET /v1/tenants/self/calls/active` (`store::active_calls`,
  world-scoped) returns every non-ended session + participants so a reconnecting
  resource rebuilds targets. `ActiveCall` contract type.
- **ICE** — `OPN_ICE_SERVERS` (JSON, default `[]`) parsed once into `Config`, echoed
  into **every** `calls.state` snapshot (§5). coturn added to the dev compose
  (host-net for the relay UDP range); README/`.env.example` document the STUN/TURN
  wiring. Video bytes go P2P/relay, never Core.
- **Contracts** — `Evt::CallsVoice` (Durable), `ice_servers` on `Evt::CallsState`,
  `VoiceAction`/`LinkHello`/`ActiveCall` types. Bindings regenerated (3 new `.ts`
  + updated `Evt`/`ServerMsg`), coverage match-test + golden wire tests extended
  (`push_calls_voice`, `push_calls_state` +ice_servers, `link_hello_shape`).

### Decisions closed during implementation (roadmap deviations, all ponytail)

1. **Link registry keyed by `world_id`, not `TenantId`** (roadmap said
   `DashMap<TenantId>`). A voice target is world-scoped and every call transition
   already holds `world_id`, so world-keying removes a world→tenant lookup from the
   call hot path. It relies on **one tenant per world** — which I made real at the
   creation site: `admin create-tenant --world <existing>` now refuses a world that
   already has a tenant (the adversarial review's keeper #2). Multi-tenant hosting
   (§17) must re-key by tenant before lifting the invariant; marked in the module doc.
2. **Hello ack reuses `ServerMsg::Ack { reply_to: 0, ok: true }`** — the design says
   "same envelope as the client protocol." Gives the resource (and tests) an
   observable "link live" signal without inventing an off-contract frame.
3. **`is_broken_combo` is a hardcoded-`false` seam.** The design's known-broken-combo
   list is empty at v1; wiring an env list that is always empty is pure YAGNI. The
   hello field + the `INCOMPATIBLE` (4409) close path are the seam; a real list slots
   in without a protocol change. `ponytail:` marked.
4. **Link `send` is local-only** (single-replica, §9). The registry is in-process;
   cross-replica link routing rides the same future as the rest of `replicas > 1`.
   Documented at the top of `link.rs`.
5. **Distinct link close codes** (4400 bad-hello / 4408 taken-over / 4409 incompatible
   / 4410 slow-consumer). 4409 is the client protocol's slow-consumer code, but the
   roadmap pins 4409 to *incompatible* on the link, so link slow-consumer is 4410 —
   no operator confuses a version reject with a full queue.

### The keeper this session (the point of rule 4): a MED leak found by the test-gap lens, missed by 9 green tests

The adversarial workflow (4 lenses — correctness / protocol / security / test-gap —
each finding then skeptically verified; **4 confirmed / 12 raw, 8 refuted**) landed
its keeper on the **independent test-author leg** again, fifth sprint running:

> An **active** call whose participants both drop their sockets *without* an explicit
> `hangup` never reaches `Ended`. A WS disconnect deliberately does not transition a
> participant row (the same fact the part-A reap keeper turned on), and the only
> reaper is `ringing`-only — so no FSM transition ever fires. The link never receives
> its matching `clear`: voice stays bound to characters no longer present, **and**
> `/calls/active` keeps re-syncing the dead call so a reconnecting FXServer re-binds
> it. The ringing state has a 60 s net; active had none.

The part-A reap keeper was "a ring the reap could never fire on"; this is its exact
mirror one state over — an **active** call the ring reap was never meant to touch,
with no equivalent net. Part B made a part-A-latent leak *observable* (the voice
lifecycle is what surfaces it). Fix (design-doc-first, §10.4 amended the same day):
a second janitor task `calls_reap_orphaned` ends active sessions whose joined
participants are **all offline** (the registry is the liveness signal SQL can't see,
so the janitor bridges: store yields candidates + joined chars, the task drops any
with a still-online participant, `end_active_orphans` ends the rest) and routes the
end through `publish_snapshot` — so the link `clear` and the truthful `/calls/active`
both fall out for free. Age-gated 60 s so a call mid-setup is spared; the
`AND state = 'active'` update guard makes it idempotent against a concurrent hangup.
The un-tested-until-now double-crash path is now `orphaned_active_call_reaped_emits_clear`
(drops both real sockets, waits for offline, ages, reaps, asserts `clear`).

The rejected alternative — "tie participant `left` to WS disconnect" — is wrong: a
mobile client reconnects (takeover) on every network blip, so disconnect≠left would
end a call on every reconnect. Call state must stay independent of socket lifecycle
(the design's own "link down = calls still connect"); a liveness-gated janitor sweep
is the right shape.

### Also fixed / documented from the review

- **LOW, fixed** — coverage ledger named a nonexistent test for `CallsVoice`
  (`set_targets_on_accept_clear_on_hangup`, missing the `_and_`). The match-arm
  strings are unused, so the compiler never caught the drift — exactly the "a test
  you didn't write is a lie" the ledger exists to prevent. Corrected. (This is the
  discipline-not-compiler-enforced drift the Sprint-5B addendum warned about, biting
  again — same class as the lapsed goldens/canary.)
- **LOW, fixed at source** — the world-key eviction (#2 above): the `admin` guard
  enforces one-tenant-per-world where tenants are born, so the world-keyed link can't
  be silently taken over by a second tenant. No schema `UNIQUE` (that would
  over-constrain the multi-tenant-hosting future the design leaves open).
- **LOW, documented as accepted ceiling** — `/link` has no pre-auth socket cap
  (unlike `/ws`). But `/ws` upgrades *before* auth, so its caps bound anonymous
  sockets; `/link` authenticates *before* upgrade (`TenantAuth` → 401 pre-upgrade),
  so the anonymous flood those caps prevent can't happen. The residual (a valid or
  leaked key opening many pre-hello sockets) is a credentialed abuse that per-IP caps
  would not reliably stop — a defensible tradeoff, documented in the module.
- **Refuted, correctly** — a reviewer claimed `emit_voice` (post-commit, outside the
  session lock) could deliver `set_targets` after `clear` under concurrency. The
  verifier traced it out: single-replica `gateway::publish` reaches no `await`
  (the `replicas > 1` branch is dead), so there is no yield between the lock-releasing
  commit and the link `try_send`, and the `FOR UPDATE` serializes commits per call —
  emit order matches commit order. Real only under `replicas > 1`, which the link is
  already documented not to support.

### Verification (rule 4)

- `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` clean (the `core`
  crate's `unwrap_used` deny respected — `?`/`.expect`/`unwrap_or_else`/`assert!` only).
- **Full workspace suite green** against the live stack (Postgres+Redis+MinIO): 29
  binaries, 0 failures. New `tests/link.rs` (9): the two-client + real-link lifecycle
  (accept→set_targets→partial-hangup→shrunk targets→clear), takeover (old link 4408 +
  successor still receives), link-down drop (call still connects), `/calls/active`
  re-sync reflecting live state, the orphan-reap keeper, decline-emits-no-voice,
  bad-hello 4400, bad-key rejection. Plus 3 `link.rs` unit tests (backpressure close,
  takeover seq-guard, connected-world-only send) and 3 golden wire tests. Adversarial
  workflow: 16 agents, 4 confirmed findings all resolved (2 fixed in code, 1 fixed at
  source, 1 documented).

### Exit criteria status (Sprint 6 — now fully closed)

| Criterion | Status |
|---|---|
| Scripted two-client + link demo: call connects, link `set_targets`, hangup clears | **PASS** — `set_targets_on_accept_and_clear_on_hangup` drives two real client sockets + a real `/link` (not a fake link half). |
| FSM pure function with 100 % transition-table coverage | **PASS** (part A). |
| All `calls.*` in coverage test; `/link` + re-sync in route test | **PASS** — `calls.voice` in the Evt match-test; `/link` and `/v1/tenants/self/calls/active` both hit against the real `app_router` in `tests/link.rs` (rule 3). |

### Reflection

- **The seam-split paid a fifth time.** Part A / part B shared no tables, so B was a
  clean, fully-reviewed slice on the committed base — same dividend Sprints 4/5 paid.
- **The independent test leg caught a shipped defect for the fifth straight sprint**,
  and it was the *mirror* of part A's keeper (WS-disconnect-doesn't-transition-a-row,
  one call-state over). Same root fact, second consequence — exactly why the budgeted
  adversarial pass (ADR-1) is work, not polish: the desk-check that "active only ends
  on hangup" reads fine until you trace the double-crash.
- **Design-doc-first held again.** §10.4 had no active-call GC policy; I amended the
  design (dated) before trusting the reaper, per the standing rule.
- **The unused-string coverage ledger drifted again** (part B's `CallsVoice` arm) —
  the same discipline-not-compiler gap the Sprint-5B addendum flagged. Worth a
  compiler-shaped fix eventually (assert the named tests exist); logged, not built.

### Not committed / next session

- **Sprint 6 (A+B) is complete and green but this B slice is untracked** on top of the
  committed 0–6A. First commit re-arms the drift gate (3 new binding files + updated
  Evt/ServerMsg) — the operator's call.
- **Sprint 7 — Ledger + exchange.** Depends only on Sprint 3 (gateway + notify), so it
  is unblocked and parallelizable; the transfer/hold FSM + nightly reconciliation is
  the next primitive.
- Still open, minor, none blocking: the deferred `calls.state` monotonic `version`
  (snapshot-vs-live reorder residual), online-member badging, `identity.me`
  own-last-seen, Bearer case-sensitivity.

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
6. **Signal authz is stricter than the roadmap's wording** *(logged post-audit)*:
   item 3 says sender and `to` must be "non-declined participants"; the code
   requires `state IN ('ringing','joined')`, which also excludes `left`. A
   participant who hung up signaling into a call they exited is nonsense, so
   stricter is right — but it is a deviation, so it's on the record. Also
   unlogged until this audit: the ring payload carries `caller_number` + `video`
   beyond the roadmap's "carrying `call_id`" (caller-ID the callee needs;
   blocked pairs never reach `start`, so no leak).

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
  (incl. the exhaustive 36-cell table test, added post-audit) and 3 golden wire
  tests.
- Both adversarial reviewers confirmed FSM faithfulness (cell-by-cell), SQL bind
  correctness, deadlock-free lock ordering, RLS/migration correctness, and wiring
  completeness — beyond the one HIGH + one MED they found.

### Exit criteria status (Sprint 6 — part A slice)

| Criterion | Status |
|---|---|
| Scripted two-client + fake-link demo (call connects, link `set_targets`, hangup clears) | **PARTIAL** — the two-client call lifecycle is `full_lifecycle_start_accept_signal_hangup` (real sockets); the fake-**link** half is part B. |
| FSM is a pure function with 100 % transition-table coverage | **PASS** — pure `apply` + `transition_table_exhaustive`: all 36 session×actor×action cells asserted against a *literal* legal-set table (not a predicate — that would mirror the implementation and prove nothing). Post-audit close: the first write-up claimed this on ~23 cells; the audit downgraded it and the exhaustive test closed it properly. Sprint 9's proptest is on top, not instead. |
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
