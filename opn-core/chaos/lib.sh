# Shared harness for the Sprint 9 chaos drills (roadmap Sprint 9 item 3).
# Sourced by each chaos/*.sh script: brings up the compose stack, builds and
# runs a release Core, mints a tenant, and tears everything down on exit. A
# drill adds only its fault injection + invariant check on top.
#
# Assumes docker + ../docker-compose.dev.yml. On the dev host the docker socket
# needs the docker group, so run the whole script under it:
#   sg docker -c 'bash chaos/kill9-mid-send.sh'

set -euo pipefail

# Repo root = the dir holding docker-compose.dev.yml (chaos/ lives beside it).
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

COMPOSE="docker compose -f docker-compose.dev.yml"
WORKDIR="$(mktemp -d)"
CORE_BIN="target/release/opn-core"
LOADGEN_BIN="target/release/opn-loadgen"
CORE_PID=""

# Core config — mirrors CI (perf-smoke.yml). Overridable from the environment.
export DATABASE_URL="${DATABASE_URL:-postgres://opn_app:opn@localhost:5432/opn}"
export OPN_MIGRATE_DATABASE_URL="${OPN_MIGRATE_DATABASE_URL:-postgres://opn_migrate:opn@localhost:5432/opn}"
export REDIS_URL="${REDIS_URL:-redis://localhost:6379}"
export S3_ENDPOINT="${S3_ENDPOINT:-http://localhost:9000}"
export S3_BUCKET="${S3_BUCKET:-opn}"
export S3_KEY="${S3_KEY:-opn}"
export S3_SECRET="${S3_SECRET:-opnsecret}"
export OPN_JWT_SECRET="${OPN_JWT_SECRET:-chaos-secret}"
export OPN_BIND="${OPN_BIND:-127.0.0.1:8080}"
export OPN_METRICS_BIND="${OPN_METRICS_BIND:-127.0.0.1:9090}"
# Second Core instance for the two-instance (replicas>1) redis-restart drill.
export OPN_BIND_B="${OPN_BIND_B:-127.0.0.1:8081}"
export OPN_METRICS_BIND_B="${OPN_METRICS_BIND_B:-127.0.0.1:9091}"
# Drills open several sockets from one IP; raise the per-IP pre-auth cap above
# the default 5 (same reason as perf-smoke.yml).
export OPN_PREAUTH_PER_IP_MAX="${OPN_PREAUTH_PER_IP_MAX:-400}"

WS_URL="ws://${OPN_BIND}/ws"
HTTP_URL="http://${OPN_BIND}"
WS_URL_B="ws://${OPN_BIND_B}/ws"
HTTP_URL_B="http://${OPN_BIND_B}"
CORE_B_PID=""

log() { echo "[chaos] $*" >&2; }

cleanup() {
  local code=$?
  core_stop_b || true
  core_stop || true
  $COMPOSE down -v >/dev/null 2>&1 || true
  rm -rf "$WORKDIR"
  exit "$code"
}
trap cleanup EXIT INT TERM

stack_up() {
  # Clean slate so the drill is repeatable (exit criterion: green three runs in
  # a row) — `create-tenant` refuses a world that already has a tenant, so a
  # leftover DB from a prior run would fail the second. Destructive to any
  # running dev stack by design; the drill owns the stack for its duration.
  log "starting compose stack (postgres, redis, minio) …"
  $COMPOSE down -v >/dev/null 2>&1 || true
  $COMPOSE up -d --wait postgres redis minio
  $COMPOSE run --rm createbucket >/dev/null
}

build_release() {
  log "building release core + loadgen …"
  cargo build --release -p opn-core -p opn-loadgen
}

core_start() {
  log "starting core …"
  nohup "$CORE_BIN" >"$WORKDIR/core.log" 2>&1 &
  CORE_PID=$!
}

# Graceful stop (cleanup path).
core_stop() {
  if [ -n "$CORE_PID" ] && kill -0 "$CORE_PID" 2>/dev/null; then
    kill "$CORE_PID" 2>/dev/null || true
    wait "$CORE_PID" 2>/dev/null || true
  fi
  CORE_PID=""
}

# The fault the kill9 drill injects: a hard, unclean crash.
core_kill9() {
  log "kill -9 core (pid $CORE_PID) …"
  kill -9 "$CORE_PID" 2>/dev/null || true
  wait "$CORE_PID" 2>/dev/null || true
  CORE_PID=""
}

# The fault the pg-restart drill injects: a Postgres outage, Core untouched.
# stop → sleep → start (not `restart`, whose ~1s bounce can be too quick to
# force pool acquire_timeouts). $1 = outage seconds (default 6, chosen > the 3s
# pool acquire_timeout so an in-gap send is guaranteed an `internal` ack). The
# pgdata volume persists across stop/start, so committed rows survive — the
# drill asserts none are lost.
pg_restart_gap() {
  local gap="${1:-6}"
  log "stopping postgres for ${gap}s (DB outage; core rides it) …"
  $COMPOSE stop postgres >/dev/null 2>&1
  sleep "$gap"
  log "starting postgres …"
  $COMPOSE up -d --wait postgres >/dev/null 2>&1
}

core_wait_health() {
  for _ in $(seq 1 60); do
    if curl -sf "$HTTP_URL/healthz" >/dev/null 2>&1; then
      log "core healthy"
      return 0
    fi
    sleep 0.5
  done
  log "core did not become healthy within 30s"
  cat "$WORKDIR/core.log" >&2 || true
  return 1
}

# ── Two-instance mode (redis-restart drill) ────────────────────────────────
# A second Core sharing the same PG+Redis+MinIO, on a different bind. The drill
# exports OPN_REPLICAS=2 first, so both instances run the fanout listener; this
# second one only overrides its bind/metrics and inherits the rest.
core_start_b() {
  log "starting core B on ${OPN_BIND_B} …"
  OPN_BIND="$OPN_BIND_B" OPN_METRICS_BIND="$OPN_METRICS_BIND_B" \
    nohup "$CORE_BIN" >"$WORKDIR/core-b.log" 2>&1 &
  CORE_B_PID=$!
}

core_stop_b() {
  if [ -n "$CORE_B_PID" ] && kill -0 "$CORE_B_PID" 2>/dev/null; then
    kill "$CORE_B_PID" 2>/dev/null || true
    wait "$CORE_B_PID" 2>/dev/null || true
  fi
  CORE_B_PID=""
}

core_b_wait_health() {
  for _ in $(seq 1 60); do
    if curl -sf "$HTTP_URL_B/healthz" >/dev/null 2>&1; then
      log "core B healthy"
      return 0
    fi
    sleep 0.5
  done
  log "core B did not become healthy within 30s"
  cat "$WORKDIR/core-b.log" >&2 || true
  return 1
}

# The fault the redis-restart drill injects: a hard Redis crash + restart.
# `kill` (SIGKILL), not `restart` (SIGTERM): a graceful stop makes redis snapshot
# its keyspace to dump.rdb and reload it, so `presence:*` would survive and the
# rebuild would never be exercised. SIGKILL with no recent save point (and no
# volume/appendonly in the dev compose) brings redis back *empty* — so the
# refresher must genuinely rewrite the keys and each Core's listener must
# re-psubscribe. Cores are never touched.
redis_restart() {
  log "hard-killing redis (SIGKILL, empty keyspace on restart) …"
  $COMPOSE kill -s KILL redis >/dev/null 2>&1
  $COMPOSE up -d --wait redis >/dev/null 2>&1
}

# `redis_cli KEYS 'presence:*'` etc. -T disables the TTY (needed in CI).
redis_cli() { $COMPOSE exec -T redis redis-cli "$@"; }

# Number of live `presence:*` keys (the refresher's output). Prints 0 when none.
presence_key_count() {
  redis_cli --scan --pattern 'presence:*' 2>/dev/null | grep -c . || true
}

# Create a tenant + world; echo its one-time API key on stdout.
mint_tenant() {
  local out key
  out="$("$CORE_BIN" admin create-tenant --name chaos --new-world chaosworld)"
  key="$(echo "$out" | sed -n 's/^api key:[[:space:]]*//p')"
  if [ -z "$key" ]; then
    log "failed to capture api key from admin output: $out"
    return 1
  fi
  echo "$key"
}
