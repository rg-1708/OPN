#!/usr/bin/env bash
# link-drop (roadmap Sprint 9 item 3).
#
# Invariant: a tenant `/link` resource that drops mid-call and reconnects must
#   (1) recover the still-active call via GET /v1/tenants/self/calls/active — the
#       re-sync route, since the link never re-emits existing calls on connect
#       (§5); and
#   (2) receive `calls.voice set_targets` again for a *subsequent* accept on the
#       reconnected link (the link delivers once more after the drop).
#
# The fault is resource-side — the link consumer crashes and reconnects — so
# unlike the redis/pg drills there is nothing for bash to inject: the
# `--link-drop` checker drops its own /link socket and reconnects it. Core is
# never touched. This script only stands up one Core + a tenant and gates on the
# checker's exit code.
#
# Exit 0 = both invariants held, 1 = one broke, 2 = setup failure. `set -e` (from
# lib.sh) propagates the checker's non-zero exit straight out.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

stack_up
build_release
core_start
core_wait_health

API_KEY="$(mint_tenant)"
export OPN_LOADGEN_API_KEY="$API_KEY"

log "link-drop: drive a call → set_targets, drop the link, reconnect, re-sync …"
"$LOADGEN_BIN" --link-drop "$HTTP_URL" "$WS_URL"

log "PASS — link re-sync recovered the active call and targets re-emitted after reconnect"
