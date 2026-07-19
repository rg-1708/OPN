#!/usr/bin/env bash
# pg-restart (roadmap Sprint 9 item 3).
#
# Invariant (three parts): a Postgres outage under load must
#   (1) never lose an acked message — Core acks a send only after commit (§8)
#       and the pgdata volume persists across the restart;
#   (2) surface *error acks, not silence* during the gap — Core stays up and
#       acks each in-gap send an `internal` when its pool can't reach PG, rather
#       than hanging the socket;
#   (3) recover — the pool reconnects and ok acks resume once PG is back.
#
# Drill: loadgen sends at 30 msg/s journaling every acked (channel, seq); mid-
# run Postgres is stopped for a gap longer than the 3s pool acquire_timeout,
# then started; loadgen runs to completion across the whole window. Its own exit
# gates (2)+(3) — `assert_error_acks` (error acks seen in the gap) plus a clean
# finish (recovered, no fatal setup error). A resuming subscribe then gates (1)
# via `--verify-resume` (every acked seq replays after the outage).
#
# Exit 0 = all three held, 1 = an invariant broke, 2 = setup failure. Same code
# convention as the load run and the kill9 drill.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

SCENARIO="crates/loadgen/scenarios/chaos-pg.json"
JOURNAL="$WORKDIR/journal.json"

stack_up
build_release
core_start
core_wait_health

API_KEY="$(mint_tenant)"
export OPN_LOADGEN_API_KEY="$API_KEY"
export OPN_LOADGEN_ACK_JOURNAL="$JOURNAL"

log "loadgen: 30 msg/s, journaling acks -> $JOURNAL"
"$LOADGEN_BIN" --scenario "$SCENARIO" >"$WORKDIR/loadgen.log" 2>&1 &
LG_PID=$!

# Let sends flow past warmup, then take Postgres down mid-stream for a gap
# longer than the pool acquire_timeout (guarantees in-gap sends get error acks),
# then bring it back. Core is never touched — it must ride the outage.
sleep 7
pg_restart_gap 6
core_wait_health # pool reconnected; /healthz green again

# loadgen ran the whole window (pre-gap ok acks, in-gap error acks, post-gap ok
# acks). Its exit gates "error acks seen in the gap" + "finished clean".
LG_CODE=0
wait "$LG_PID" || LG_CODE=$?
if [ "$LG_CODE" -ne 0 ]; then
  log "FAIL — loadgen exited $LG_CODE (no error acks in the gap, or a fatal error)"
  cat "$WORKDIR/loadgen.log" >&2 || true
  exit "$LG_CODE"
fi

log "verifying every acked message replays after the DB outage …"
"$LOADGEN_BIN" --verify-resume "$JOURNAL" "$WS_URL"
