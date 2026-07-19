#!/usr/bin/env bash
# kill9-mid-send (roadmap Sprint 9 item 3).
#
# Invariant: a `kill -9` of Core between an ack and a restart loses no acked
# message. Core acks a send only after the row commits (persist-then-ack, §8),
# and resume replays every committed message (§4.4) — so every ok-acked seq must
# reappear on a resuming subscribe after a hard crash + restart.
#
# Drill: loadgen sends at 30 msg/s journaling every acked (channel, seq); `kill
# -9` Core mid-stream; restart; a fresh member connection resumes each channel
# from seq 0 and the verifier asserts every acked seq replayed.
#
# Exit 0 = invariant held, 1 = an acked message was lost (or none recorded),
# 2 = setup failure. Loadgen's own post-kill errors are expected and ignored —
# the verifier is the sole gate.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

SCENARIO="crates/loadgen/scenarios/chaos-kill9.json"
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

# Let sends flow well past warmup, then hard-crash Core mid-stream. The window
# is generous so the journal carries a solid batch of acks even under CI timing
# jitter (the empty-journal path is a FAIL, so undershooting must not happen).
sleep 9
core_kill9

# Loadgen's live sockets are now dead; it writes the journal of pre-kill acks
# and exits with a non-zero (socket-error) code — expected, ignore it.
wait "$LG_PID" 2>/dev/null || true

core_start
core_wait_health

log "verifying every acked message replays after restart …"
"$LOADGEN_BIN" --verify-resume "$JOURNAL" "$WS_URL"
