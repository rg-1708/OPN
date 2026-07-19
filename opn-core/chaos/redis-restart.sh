#!/usr/bin/env bash
# redis-restart (roadmap Sprint 9 item 3).
#
# Invariant (two-instance mode): restarting Redis under a live two-Core topology
# must
#   (1) resubscribe pub/sub — a message sent on Core A still fans out to a
#       subscriber on Core B via Redis after the bounce (each Core's listener
#       reconnects and re-psubscribes `opn:*`, §3/§8);
#   (2) rebuild presence keys within one heartbeat cycle — the dev Redis has no
#       persistence, so its keyspace is empty right after the restart; the
#       presence refresher must rewrite `presence:*` for still-live characters
#       (§4.2).
#
# Drill: two Core instances (A:8080, B:8081) share one Redis with OPN_REPLICAS=2.
# The `--xinstance` checker holds a sender on A and a subscriber on B open across
# the whole window: it proves the A→B hop once, then idles while this script
# bounces Redis and watches `presence:*` repopulate, then proves the hop again —
# the second proof is the resubscribe gate. Presence rebuild is asserted here via
# redis-cli; the checker's exit code gates the two cross-instance deliveries.
#
# Exit 0 = both invariants held, 1 = one broke, 2 = setup failure.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# Two-instance mode: both Cores run the fanout listener. A shorter heartbeat than
# the 30s default keeps the drill quick without changing what is tested — the
# refresher's cadence *is* "one heartbeat cycle", whatever its length.
export OPN_REPLICAS=2
export OPN_HEARTBEAT_SECS="${OPN_HEARTBEAT_SECS:-10}"

HEARTBEAT="$OPN_HEARTBEAT_SECS"
SETTLE=$((HEARTBEAT * 2 + 15)) # checker idles this long between its two deliveries
REBUILD_DEADLINE=$((HEARTBEAT + 10))

stack_up
build_release
core_start # instance A (:8080), OPN_REPLICAS=2 from the export above
core_wait_health
core_start_b # instance B (:8081)
core_b_wait_health

API_KEY="$(mint_tenant)"
export OPN_LOADGEN_API_KEY="$API_KEY"

log "xinstance: sender on A ($WS_URL), subscriber on B ($WS_URL_B), settle ${SETTLE}s"
"$LOADGEN_BIN" --xinstance "$HTTP_URL" "$WS_URL" "$WS_URL_B" "$SETTLE" \
  >"$WORKDIR/xinstance.log" 2>&1 &
XI_PID=$!

# Wait for the checker's pre-restart delivery proof before injecting the fault.
log "waiting for pre-restart cross-instance delivery …"
for _ in $(seq 1 120); do
  if grep -q "PRE delivery OK" "$WORKDIR/xinstance.log" 2>/dev/null; then
    break
  fi
  if ! kill -0 "$XI_PID" 2>/dev/null; then
    log "FAIL — checker exited before the pre-restart delivery proof"
    cat "$WORKDIR/xinstance.log" >&2 || true
    exit 1
  fi
  sleep 0.5
done
if ! grep -q "PRE delivery OK" "$WORKDIR/xinstance.log" 2>/dev/null; then
  log "FAIL — no pre-restart delivery proof within 60s"
  cat "$WORKDIR/xinstance.log" >&2 || true
  exit 1
fi

# Both Cores hold a live character each (share_presence on), so keys exist now.
BEFORE="$(presence_key_count)"
log "presence keys before restart: $BEFORE"
if [ "$BEFORE" -lt 1 ]; then
  log "FAIL — no presence keys before the restart (baseline broken, drill vacuous)"
  exit 1
fi

redis_restart

# Non-vacuous evidence: the ephemeral dev Redis comes back empty. Logged, not
# gated — a refresher tick could sneak in within the sub-second read window.
log "presence keys immediately after restart: $(presence_key_count)"

# (2) presence keys must rebuild within one heartbeat cycle (+ margin). Only the
# refresher can do this — on_connect already fired for the held characters — so
# the reappearance genuinely tests the rebuild path.
log "waiting up to ${REBUILD_DEADLINE}s for presence keys to rebuild …"
REBUILT=0
for _ in $(seq 1 $((REBUILD_DEADLINE * 2))); do
  if [ "$(presence_key_count)" -ge 1 ]; then
    REBUILT=1
    break
  fi
  sleep 0.5
done
if [ "$REBUILT" -ne 1 ]; then
  log "FAIL — presence keys did not rebuild within ${REBUILD_DEADLINE}s of the redis restart"
  exit 1
fi
log "presence keys rebuilt within one heartbeat cycle: $(presence_key_count)"

# (1) the checker's post-restart delivery + clean exit gate the resubscribe.
log "waiting for the checker's post-restart delivery …"
XI_CODE=0
wait "$XI_PID" || XI_CODE=$?
if [ "$XI_CODE" -ne 0 ]; then
  log "FAIL — cross-instance delivery broke across the restart (checker exit $XI_CODE)"
  cat "$WORKDIR/xinstance.log" >&2 || true
  exit "$XI_CODE"
fi

log "PASS — pub/sub resubscribed and presence keys rebuilt across the redis restart"
