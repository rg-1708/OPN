# perf/results — committed performance runs

Measurement discipline for the Sprint 10 perf environment (roadmap Sprint 10
item 2). Each scenario run writes its one-line JSON summary here, committed as:

    <YYYY-MM-DD>-<scenario>.json      e.g. 2026-07-20-design.json

`just perf` produces today's `design` result automatically.

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
5. Reconnect-storm: every connection resumed in < 60s with no replay gaps.
6. Core RSS ≤ 200MB at the design load.
