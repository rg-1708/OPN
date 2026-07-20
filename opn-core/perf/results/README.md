# perf/results — committed performance runs

Measurement discipline for the Sprint 10 perf environment (roadmap Sprint 10
item 2). Each scenario run writes its one-line JSON summary here, committed as:

    <YYYY-MM-DD>-<scenario>.json      e.g. 2026-07-20-design.json

`just perf` produces today's `design` result automatically.

## Scenarios

- `design` — steady-state realistic load (300 conns, 30 msg/s, 5 min).
- `soak-short` — routine/dev soak: design load (300 conns, 30 msg/s) for 2h.
  ~24x the nightly smoke — long enough to surface RSS creep / fd leaks / janitor
  accumulation that the 5-min smoke cannot, but light enough to run often on the
  dev host. The full 24h soak below stays the once-per-release gate; this is the
  one you run week to week.
- `soak10x` — the §15 release-gate soak: 10x design (3000 conns, 300 msg/s) for
  24h. Loadgen asserts only the delivery invariants (no seq gaps, no durable
  closes); the item-6 soak targets below are operator-observed, and the ack-p99
  gate stays omitted until item 7 measures it. A 24h run at this rate records
  tens of millions of latency samples, so the loadgen now aggregates into an
  `hdrhistogram` (bounded, fixed-memory buckets) rather than the unbounded
  sorted `Vec` of per-sample latencies it used at v0 — the sorted `Vec` was fine
  to ~1M samples but would grow to hundreds of millions over a soak, the
  latent-OOM flagged in `crates/loadgen/src/main.rs`. That is what makes the
  soak memory-safe.
- `reconnect-storm` — thundering-herd resume test: 10x/3000-conn design-rate load
  runs, then at a set instant every connection drops its socket and reconnects
  with a 0–3s stagger and resumes via `sub last_seq`. The loadgen tracks seq
  continuity per client THROUGH the reconnect (`SeqTracker`), so `assert_no_seq_gaps`
  fails if a resume dropped a message — roadmap gate 5's "no replay gaps". Resume
  latency is measured (`resume_*` fields in the summary) but not hard-gated until
  item 7. `assert_reconnected` guards against a vacuous pass where the storm never
  fired. Committed at the real 3000-conn spec; the heavy run is the operator's
  pinned-hardware pass (soak10x precedent).
- `hot-channel` — the fan-out stress shape: one ~100-member group channel (conn 0
  creates it and `member_add`s the rest, paced under the Social rate budget), all
  members subscribe to the single channel and send at 10 msg/s aggregate, so each
  send fans out to ~100 subscribers ≈ 1000 evt/s. Gates `assert_no_seq_gaps`
  (delivery continuity must hold under the heavy fan-out) and `assert_no_durable_closes`
  (the 100-way fan-out must never slow-consumer-close a healthy socket). Committed at
  the real 100-conn spec; the heavy run is the operator's pinned-hardware pass. Run:
  `just perf hot-channel`.
- `call-churn` — the call-lifecycle stress shape: 50 caller/callee pairs (one task
  per pair) each cycle a full call (`calls.start` → `calls.accept` → `calls.signal`
  both ways → `calls.hangup`) at ~1 Hz — ~50 calls/s aggregate — while a single
  `/link` consumer drains the tenant's `set_targets`/`clear` voice-target events.
  Exercises the call FSM (start/accept/hangup transitions, session row locking) and
  the `/link` relay under load. Gates `assert_calls` (non-vacuity: some call completed
  AND the link received `set_targets`) and `assert_no_durable_closes` (the link
  consumer and every party socket must survive the churn without a 4409). The `ack`
  percentiles here measure call-setup latency, not message RTT. Committed at the real
  100-conn spec; the heavy run is the operator's pinned-hardware pass. Run:
  `just perf call-churn`.

## Read the trend, not the run

A single run is noise — machine thermals, background load, and warm-up all move
the numbers. The **trend across committed runs is the artifact**: a p99 that
creeps up over a week is the signal, one slow afternoon is not. That is why the
summaries are committed to git rather than uploaded and discarded.

## Isolate the generator

The loadgen steals whatever CPU it runs on. Run it from a **separate machine**,
or at minimum on a **separate pinned core set** from Core. On the dev host
(i5-14500 hybrid CPU) the `perf` recipe pins Core to the E-cores (`taskset -c
12-19`) and leaves the loadgen and OS on the P-cores, so the generator never
steals the cores it is measuring. Verify your topology with `lscpu -e` and
override `PERF_CPUSET` in the justfile if your numbering differs.

## Sprint 10 gate targets (roadmap item 6)

The runs collected here exist to hold these six gates:

1. Command p99 < 5ms at the design load.
2. Zero durable closes at the design load.
3. Zero durable closes at soak10x.
4. soak10x (24h): RSS slope ≈ 0, fd count flat, and p99 in hour 24 within 20%
   of hour 1.
5. Reconnect-storm: every connection resumed in < 60s with no replay gaps
   (machinery: `reconnect-storm.json` + the loadgen `SeqTracker` continuity check;
   the < 60s threshold is measured, not yet gated — item 7).
6. Core RSS ≤ 200MB at the design load.
