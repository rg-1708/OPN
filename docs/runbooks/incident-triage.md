# Runbook: incident triage

Fixed order (OPN-CORE.md [§14](../OPN-CORE.md)): **`/healthz` → `/metrics` → logs-by-span.**
Do them in order; each narrows the next. `$BIND` = `OPN_BIND` (public HTTP/WS),
`$METRICS` = `OPN_METRICS_BIND` (internal-only) — both required at boot
([config.rs:88](../../opn-core/crates/core/src/config.rs)).

## 1. `/healthz` — is the process live and are its stores reachable?

Live PG `SELECT 1` + Redis `PING`, 1 s timeout each; `200` iff **both** pass,
else `503` ([http/mod.rs:48](../../opn-core/crates/core/src/http/mod.rs)).
The JSON body is returned on **both** paths — use it to confirm which build is live:

```bash
curl -s -o /tmp/hz -w 'HTTP %{http_code}\n' http://$BIND/healthz && jq . /tmp/hz
# {"status":"ok"|"unavailable","contracts_version":"…","core_version":"…"}
```

- `200 / status:"ok"` → PG **and** Redis both live. App layer is up; go to `/metrics`.
- `503 / status:"unavailable"` → PG **or** Redis is down. Which one is in the log,
  not the body: grep `healthz failing` (fields `pg_ok`, `redis_ok`). Coolify gates
  rollout on this, so a `503` also blocks new deploys.
- `contracts_version` (`contracts::CONTRACTS_VERSION`) / `core_version`
  (`CARGO_PKG_VERSION`) confirm the running build without shelling in — check these
  first when triaging right after a deploy.
- Connection refused / no response → process down or wrong `OPN_BIND`; check the
  supervisor/Coolify, not the code.

## 2. `/metrics` — Prometheus text on the internal bind

```bash
curl -s http://$METRICS/metrics | grep -E '^opn_'
```

| Metric | Type / labels | Read it for |
|---|---|---|
| `opn_connections` | gauge | live authenticated WS connections ([registry.rs:175](../../opn-core/crates/core/src/gateway/registry.rs)) |
| `opn_command_seconds` | histogram `{cmd}` | handler latency; the §7 p99 ≤ 25 ms target is watched here ([dispatch.rs:51](../../opn-core/crates/core/src/gateway/dispatch.rs)) |
| `opn_sendq_drops_total` | counter `{class}` | dropped/closed send queues; `class` ∈ `durable_close` \| `ephemeral` \| `link_close`. Any `durable_close` > 0 is alert-worthy (§14) |
| `opn_janitor_runs_total` | counter `{task,outcome}` | background sweeps; watch `outcome="err"` per `task` ([janitor.rs](../../opn-core/crates/core/src/janitor.rs)) |
| `opn_ledger_drift_total` | counter | reconciliation froze a drifted account — see [frozen-account.md](frozen-account.md). Should be flat 0 |
| `opn_sendq_depth` | gauge | **registered, not yet sampled — reads 0** ([observe.rs:20](../../opn-core/crates/core/src/observe.rs)); no live emitter landed yet |
| `opn_pg_pool_in_use` | gauge | in-use pool connections, **sampled every 30 s** by the janitor ([janitor.rs](../../opn-core/crates/core/src/janitor.rs), `pool_in_use`); coarse (30 s instantaneous), so brief spikes may not show — cross-check PG `pg_stat_activity` for live detail. The `PgPoolExhaustion` alert (≥ 20 for 1 min) keys on it |

Also present: `opn_commands_total{cmd,outcome}`, `opn_inbox_inserts_total`.

## 3. Logs — structured JSON, filtered by span field

`tracing` → JSON on stdout (Coolify collects). One span per command; payload bodies
are never logged (§14). Command spans carry `cmd, tenant, world, char, outcome`
(+ `duration`); the janitor span carries `task`. Filter with `jq`:

```bash
# all failed commands for one tenant
… | jq -c 'select(.span.tenant=="<uuid>" and .span.outcome=="err")'
# one command type across a world
… | jq -c 'select(.span.cmd=="channels.send" and .span.world=="<uuid>")'
# a single character's activity
… | jq -c 'select(.span.char=="<uuid>")'
# janitor failures
… | jq -c 'select(.span.task!=null and .level=="ERROR")'
```

## Symptom → first metric to check

| Symptom | Look first at |
|---|---|
| Clients can't connect / dropped at handshake | `/healthz` (PG or Redis down), then `opn_connections` |
| Slow acks / latency complaints | `opn_command_seconds` p99 by `cmd` |
| Messages not delivered / lost pushes | `opn_sendq_drops_total{class}` (esp. `durable_close`) |
| Money op rejected with `conflict` | `opn_ledger_drift_total`; then [frozen-account.md](frozen-account.md) |
| Cleanup / expiry not happening | `opn_janitor_runs_total{outcome="err"}` by `task` |
| Suspected DB pool exhaustion | `opn_pg_pool_in_use` (30 s sample; near 20 = saturated), then PG `pg_stat_activity` for live detail |
| Right after a deploy, odd behavior | `/healthz` body `core_version` / `contracts_version` — confirm the live build |
