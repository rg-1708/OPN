# Runbook: 1 → 2 replica scale-up

Core defaults to a single replica (OPN-CORE.md [§7.1](../OPN-CORE.md), §3). Going to
two is a config + reverse-proxy change, not a code change — the cross-instance fan-out
already exists. Both instances share **one Postgres and one Redis**; nothing is
sharded.

## What 2 replicas turns on

`OPN_REPLICAS` defaults to `1` ([config.rs:103](../../opn-core/crates/core/src/config.rs)).
Setting it `> 1`:

- spawns the Redis pub/sub fan-out listener at boot —
  `main.rs:81 → fanout::spawn_listener` ([main.rs:81](../../opn-core/crates/core/src/main.rs),
  [fanout.rs:69](../../opn-core/crates/core/src/gateway/fanout.rs));
- makes `gateway::publish` also `PUBLISH` to Redis after the local fan-out —
  gated `if replicas > 1` ([gateway/mod.rs:22](../../opn-core/crates/core/src/gateway/mod.rs)).

Channel scheme: `opn:<world>:<topic>` (prefix `opn:`,
[fanout.rs:23](../../opn-core/crates/core/src/gateway/fanout.rs)); the listener
`PSUBSCRIBE opn:*`, deserializes the payload, and replays it into its **local**
registry via `publish_local` — no DB read on the subscriber side. Messages carry a
replica id so the origin drops its own echo. Fan-out is **best-effort** (a failed
`PUBLISH`/dropped subscriber is logged, not retried); durability is the persist-then-ack
row + resume/inbox, not the live push.

Presence already works cross-instance: the Redis keys `presence:<world>:<character>`
([presence.rs:20](../../opn-core/crates/core/src/gateway/presence.rs)) are shared, so
replica 2 reads the same online-truth replica 1 wrote.

## Prerequisite: sticky WebSocket routing (reverse-proxy layer)

Handlers process a connection's commands in-process and assume a character's live
session stays on one instance (OPN-CORE.md:686; cross-replica `/link` relay routing is
still future — [link.rs:22](../../opn-core/crates/core/src/gateway/link.rs)). So the
reverse proxy (Traefik under Coolify) **must** pin each WS session to one instance.

**(planned — not in this repo.)** No Traefik/Coolify config lives in `opn-core`; sticky
routing is configured at the deploy layer. In Traefik terms that is a `sticky` cookie
on the service load-balancer for the WS route (`traefik.http.services.<svc>.loadBalancer.sticky.cookie`),
or the equivalent Coolify "sticky sessions" toggle. Configure it **before** raising
`OPN_REPLICAS`; without it, a reconnect can land on the other instance and lose
in-process link state.

## Steps

1. Bring up a second Core with **identical** stateful config — same `DATABASE_URL`,
   `OPN_MIGRATE_DATABASE_URL`, `REDIS_URL`, `OPN_JWT_SECRET`, `S3_*` — and its **own**
   `OPN_BIND` / `OPN_METRICS_BIND` (distinct ports/addr). Migrations run at startup
   against the shared owner URL and are idempotent; a second boot is safe.
2. Set `OPN_REPLICAS=2` on **both** instances.
3. Enable sticky WS at the reverse proxy (above).
4. Redeploy/restart both; confirm each passes `/healthz` (Coolify gates on it).

## Verify

- **Both up:** each instance's `/metrics` shows `opn_connections > 0` under real load.
- **Fan-out crosses instances (in-process test, already exists):**
  ```bash
  cargo test -p opn-core --test fanout cross_replica_fanout_and_self_drop
  ```
  Two in-process replicas sharing one pool; asserts A→B delivery + origin self-drop
  ([tests/fanout.rs:28](../../opn-core/crates/core/tests/fanout.rs)).
- **Live two-box check:** the loadgen cross-instance checker — a message sent on
  instance A must reach a subscriber on instance B:
  ```bash
  OPN_LOADGEN_API_KEY=… opn-loadgen --xinstance <http> <ws_A> <ws_B> [settle_secs]
  # → "xinstance: PRE delivery OK" … "xinstance: PASS"
  ```
  ([loadgen/src/xinstance.rs:1](../../opn-core/crates/loadgen/src/xinstance.rs)).
- **Full two-instance chaos drill** (two Cores A:8080/B:8081, `OPN_REPLICAS=2`, kills
  Redis and asserts resubscribe + presence rebuild): `bash chaos/redis-restart.sh`
  (part of `just chaos`).
