#!/usr/bin/env bash
# livekit-down (opn-group-calls.md G3 chaos).
#
# Invariant: killing the LiveKit SFU degrades ONLY group-call *media*. The Core
# process, the data plane, 1:1 calls, and the group *control plane* are all
# unaffected — Core mints group tokens in-process and never calls the LiveKit
# server synchronously (opn-group-calls.md §G3), so a dead SFU cannot stall or
# fail a group command. Media (client<->SFU) is the only casualty, and that is
# unobservable from Core — so this drill asserts the contrapositive: everything
# that is NOT group media survives.
#
# Drill:
#   1. full dev stack + LiveKit up; Core with LIVEKIT_* configured (group on).
#   2. baseline: --group-probe (create+join+token) passes with the SFU alive.
#   3. SIGKILL the livekit container.
#   4. --group-probe STILL passes -> group control plane decoupled from SFU
#      liveness (the regression guard: a synchronous LiveKit call in create/join
#      would hang or error right here).
#   5. --link-drop passes         -> 1:1 call FSM + /link relay unaffected.
#   6. Core stayed healthy (healthz 200, process alive) across the kill.
#   7. restart LiveKit; Core still healthy (webhook sink still served).
#
# Exit 0 = SFU outage isolated to group media, 1 = something else broke, 2 = setup.
#
# Run under the docker group on the dev host:
#   sg docker -c 'bash chaos/livekit-down.sh'

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# Enable group calls: Core reads these three; values match the dev livekit keys
# (docker-compose.dev.yml `keys: devkey: devsecret`, port 7880).
export LIVEKIT_URL="${LIVEKIT_URL:-ws://localhost:7880}"
export LIVEKIT_API_KEY="${LIVEKIT_API_KEY:-devkey}"
export LIVEKIT_API_SECRET="${LIVEKIT_API_SECRET:-devsecret}"

# LiveKit uses host networking (UDP range) and ships no compose healthcheck, so
# poll its signal port rather than `--wait`.
livekit_up() { $COMPOSE up -d livekit >/dev/null 2>&1; }
livekit_kill() {
  log "SIGKILL livekit (SFU outage) …"
  $COMPOSE kill -s KILL livekit >/dev/null 2>&1 || true
}
livekit_wait() {
  for _ in $(seq 1 60); do
    if (exec 3<>/dev/tcp/localhost/7880) 2>/dev/null; then
      exec 3>&- 2>/dev/null
      log "livekit up (:7880)"
      return 0
    fi
    sleep 0.5
  done
  log "livekit did not open :7880 within 30s"
  return 1
}

group_probe() { "$LOADGEN_BIN" --group-probe "$HTTP_URL" "$WS_URL"; }

stack_up
livekit_up
build_release
core_start
core_wait_health
livekit_wait

API_KEY="$(mint_tenant)"
export OPN_LOADGEN_API_KEY="$API_KEY"

# ── baseline: the group control plane works with the SFU alive ──────────────
log "baseline group-probe (SFU up) …"
if ! group_probe; then
  log "FAIL — baseline group-probe failed before any fault (drill vacuous)"
  exit 1
fi
log "baseline OK"

# ── fault: kill the SFU ─────────────────────────────────────────────────────
livekit_kill

# (1) group control plane must STILL answer — Core mints tokens without the SFU.
log "group-probe with the SFU dead (control-plane decoupling) …"
if ! group_probe; then
  log "FAIL — group create/join broke with the SFU down; the control plane is NOT decoupled from SFU liveness"
  exit 1
fi
log "group control plane survived the SFU kill"

# (2) 1:1 calls + /link relay must be untouched by the SFU death. --link-drop
# drives a full 1:1 call lifecycle (start/accept/signal/hangup) through the link
# relay, so a green run is the "1:1 + data plane unaffected" proof.
log "link-drop drill with the SFU dead (1:1 + data plane isolation) …"
if ! "$LOADGEN_BIN" --link-drop "$HTTP_URL" "$WS_URL" >"$WORKDIR/linkdrop.log" 2>&1; then
  log "FAIL — 1:1 call / link relay broke while the SFU was down"
  cat "$WORKDIR/linkdrop.log" >&2 || true
  exit 1
fi
log "1:1 calls + data plane unaffected"

# (3) Core never crashed and still serves — LiveKit must be a sidecar, not a
# hard dependency.
if ! curl -sf "$HTTP_URL/healthz" >/dev/null 2>&1; then
  log "FAIL — Core /healthz not serving after the SFU kill"
  exit 1
fi
if [ -z "$CORE_PID" ] || ! kill -0 "$CORE_PID" 2>/dev/null; then
  log "FAIL — Core process died on the SFU kill (LiveKit must not be a hard dependency)"
  exit 1
fi

# (4) restart the SFU; Core is still up and its webhook sink still serves.
livekit_up
livekit_wait
if ! curl -sf "$HTTP_URL/healthz" >/dev/null 2>&1; then
  log "FAIL — Core unhealthy after the livekit restart"
  exit 1
fi

log "PASS — LiveKit outage degraded only group media; control plane, 1:1, and data plane held"
