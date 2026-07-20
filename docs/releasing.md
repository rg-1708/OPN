# Releasing OPN-CORE

The release ritual and the verification cadence that runs without a human.
Roadmap Sprint 11 item 7. Companion: [contracts-semver.md](contracts-semver.md)
(what a version *number* means), OPN.md §15 (the soak/chaos gates this
sequence enforces).

Two audiences: the **release ritual** (once per version, below) and the
**standing cadence** (nightly + weekly CI, already wired). Everything here is
grounded in the actual workflow files under
[`.github/workflows/`](../.github/workflows/) and the recipes in
[`opn-core/justfile`](../opn-core/justfile) — no invented jobs.

Legend: **[auto]** runs in CI on a trigger; **[operator]** a human step on
pinned hardware or a hosting console; there is no automation for it today and
pretending otherwise is how a release breaks.

---

## The release ritual

Run in order. Each gate must be green before the next. The sequence is
roadmap item 7: tag → CI (full suite + fuzz smoke) → 24 h soak → chaos suite →
deploy → post-deploy smoke.

### 1. Tag **[operator]**

Bump `crates/contracts` and workspace versions if the wire surface changed
(see [contracts-semver.md](contracts-semver.md)), commit, then:

```sh
git tag opn-core-vX.Y.Z
git push origin opn-core-vX.Y.Z
```

The tag is the artifact of record. Nothing downstream is wired to the tag yet
— the npm publish-on-tag pipeline is *planned, Sprint 11 part B* (see
contracts-semver.md). For now the tag pins the commit the rest of the ritual
verifies.

### 2. CI full suite + fuzz smoke

**Full suite [auto]** — pushing the tag's commit runs
[`ci.yml`](../.github/workflows/ci.yml) (`name: CI`, on every push/PR). Six
jobs, all must be green:

| Job | What it gates |
|-----|---------------|
| `fmt` | `cargo fmt --check` |
| `clippy` | `cargo clippy --all-targets -- -D warnings` |
| `test` | `cargo test --workspace` against real Postgres/Redis/MinIO |
| `contracts-drift` | regenerated `.ts` bindings match the committed ones (see contracts-semver.md) |
| `sqlx-prepare` | `cargo sqlx prepare --workspace --check` (offline query cache current) |
| `cargo-deny` | `cargo deny check` (advisories, licenses, bans) |

**Fuzz smoke [auto, on demand]** — the fuzz targets are *not* in `ci.yml`
(each target is a 5-min libFuzzer burn, too slow for push/PR). They live in the
`fuzz` job of [`nightly-verify.yml`](../.github/workflows/nightly-verify.yml)
(`name: Nightly verify`). For a release, dispatch that workflow (Actions →
Nightly verify → Run workflow) or run the equivalent locally:

```sh
cd opn-core
just fuzz 300      # 3 targets × 5 min each — the CI burn (default 60s = quick local pass)
```

A crash writes an artifact and fails the job — a crash *is* the bug (§15). If
you touched invariant logic, also run the property suites at full count:

```sh
just proptest      # 1024 cases against real Postgres (nightly CI runs 256)
```

### 3. 24 h soak — `soak10x` **[operator]**

The §15 release-gate soak. **Not in CI** — it needs the pinned perf host (the
i5-14500, Core on the E-cores) and 24 h of wall clock, neither of which a GitHub
runner has. Run it on the perf host, under the docker group:

```sh
cd opn-core
sg docker -c 'just perf soak10x'   # 3000 conns, 300 msg/s aggregate, 86400 s
```

loadgen self-asserts the delivery invariants (`assert_no_seq_gaps`,
`assert_no_durable_closes`) and exits non-zero on breach. The soak *targets*
from roadmap Sprint 10 item 1 are **operator-observed**, not loadgen
assertions — watch the `/metrics` and confirm across the run:

- RSS slope ≈ 0 (no leak),
- fd count flat,
- p99 in hour 24 within 20 % of hour 1,
- zero janitor failures.

Summary JSON lands in `perf/results/<date>-soak10x.json` with loadgen's exit
code preserved. Commit it (roadmap §14 exit criterion: numbers in `perf/`).

### 4. Chaos suite

Weekly in CI (see cadence below), but re-run against the release commit:

```sh
cd opn-core
sg docker -c 'just chaos'   # kill9-mid-send, pg-restart, redis-restart, link-drop
```

Each script brings up its own compose stack + release Core and asserts a
delivery guarantee by exit code. A red run means a guarantee broke across a
fault — do not ship. Same four scripts CI runs, so a green
`Chaos drills` run on the tag's commit substitutes for the local run.

### 5. Deploy **[operator]**

Coolify, on the production host. `/healthz` gates the rollout: Coolify holds
traffic until the new container returns `200`. This is a hosting-console action
— there is no tag-triggered deploy. The deploy/rollback runbook with the exact
console steps ships with the Coolify config in **Sprint 11 part B** *(planned —
not yet written)*; until then the deploy is done by whoever set up the host. The
other operational runbooks are live in [`docs/runbooks/`](runbooks/).

### 6. Post-deploy smoke **[operator]**

Confirm the live build is the one you tagged, without shelling in — `/healthz`
carries the versions (roadmap Sprint 11 item 6, live now; see
contracts-semver.md):

```sh
curl -sf https://<host>/healthz | jq '{status, contracts_version, core_version}'
# status == "ok"; core_version == the tag; contracts_version == crate version
```

Then one message round-trip over the real wire (a `/link` hello returns the
same `contracts_version` in its ack payload) to prove send→durable→read on the
production stack. A packaged post-deploy smoke script is Sprint 11 work; until
it lands, the `curl` above plus one manual read is the gate.

---

## Standing cadence (already wired)

What runs without anyone tagging. All schedules are UTC (GitHub cron). All are
`workflow_dispatch`-able for a manual run.

### Nightly

| Workflow | Schedule | What runs |
|----------|----------|-----------|
| [`perf-smoke.yml`](../.github/workflows/perf-smoke.yml) — `Perf smoke` | `0 3 * * *` (daily 03:00) | `perf-smoke` job: release Core + loadgen `smoke.json`, ~5.5 min, self-asserting on the 25 ms p99 gate. Runs as the `opn_app` (RLS-enforced) role, like prod. |
| [`nightly-verify.yml`](../.github/workflows/nightly-verify.yml) — `Nightly verify` | `0 4 * * *` (daily 04:00) | `proptest` job (256 cases, real PG) **and** `fuzz` job (3 targets × 5 min). |

### Weekly

| Workflow | Schedule | What runs |
|----------|----------|-----------|
| [`chaos.yml`](../.github/workflows/chaos.yml) — `Chaos drills` | `0 4 * * 1` (**Mondays** 04:00) | `chaos` job: `kill9-mid-send`, `pg-restart`, `redis-restart`, `link-drop` — each asserts a delivery guarantee by exit code. |

### Honest gaps

- **The "1 h mini-soak" is not wired in CI.** Roadmap item 7 pairs the weekly
  chaos run with a mini-soak; the workflow files have chaos weekly but no soak
  on any schedule. The nightly `Perf smoke` is a ~5.5-min burst, not a soak.
  The building block exists — `crates/loadgen/scenarios/soak-short.json` (2 h,
  design load) via `sg docker -c 'just perf soak-short'` — but it runs only
  when an operator (or a host cron) invokes it. Treat the weekly cadence as
  **chaos-only** until a soak workflow is added.
- **"Pages on red" is GitHub-notification-only.** A failed scheduled run shows
  red and fires GitHub's workflow-failure email/notifications; there is no
  dedicated pager wired in these workflow files. Roadmap item 7's "verify the
  schedule actually fires and pages" is satisfied by the cron entries above
  plus GitHub's own failure surfacing — a real pager is alerting work (Sprint
  11 item 4), separate from this cadence.

---

## Command reference

All from [`opn-core/justfile`](../opn-core/justfile); run in `opn-core/`. On the
dev/perf host, the compose-orchestrating recipes need the docker group:
`sg docker -c '<recipe>'`.

| Command | Recipe |
|---------|--------|
| `just fuzz [secs=60]` | smoke every cargo-fuzz target for `secs` (nightly = 300) |
| `just proptest [cases=1024]` | property suites at full case count, real PG |
| `just chaos` | the four chaos drills |
| `just perf soak10x` | the 24 h 10× release-gate soak |
| `just perf soak-short` | the 2 h routine soak (design load) |
| `just perf [design]` | the production-shaped perf run (default scenario) |
| `just test` | `cargo test` with the migrate role wired for `sqlx::test` |
