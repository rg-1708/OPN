#!/usr/bin/env bash
# Sprint 11 item 3 — the restore DRILL. "An untested backup is a wish, not a
# backup" (roadmap §3): this scripts one full backup → lose the DB → restore
# into a fresh stack → verify cycle and asserts the result by exit code.
#
# It reuses pg-backup.sh and pg-restore.sh (so the real operational scripts are
# what's exercised) and models the actual failure domain: only the Postgres
# volume is destroyed; the backup lives in MinIO, which survives — because a
# backup in the same failure domain as the DB it protects is not a backup.
#
# Runs entirely in a throwaway compose project on isolated volumes, so it never
# touches the dev or prod stack. Needs Docker; on the dev host run it under the
# docker group:  sg docker -c 'bash deploy/backup/restore-drill.sh'
#
# Proof chain, all asserted:
#   - _sqlx_migrations row count survives the round-trip (⇒ Core restart is a
#     no-op migrate, the real recovery path works)
#   - the seeded message row survives (row-count smoke)
#   - opn_app (least-priv, real password, over TCP) reads the message back with
#     its world set, and reads ZERO with a different world (RLS intact post-restore)
#   - Core boots Healthy against the restored DB and serves as opn_app
set -euo pipefail

PROJECT=opn-restore-drill

# Throwaway credentials — this stack is created and destroyed by this script.
export OPN_COMPOSE_PROJECT="$PROJECT"
export OPN_COMPOSE=docker-compose.prod.yml
export OPN_DB_MIGRATE_PASSWORD=drillmigrate
export OPN_DB_APP_PASSWORD=drillapp
export OPN_S3_KEY=drillkey
export OPN_S3_SECRET=drillsecret123
export OPN_JWT_SECRET=drilljwt
export OPN_DOMAIN=drill.local
export OPN_ICE_SERVERS='[]'
export OPN_TURN_USER=drill
export OPN_TURN_PASSWORD=drill
export OPN_RUST_LOG=warn

# Backup target = the in-compose MinIO reached over the stack network. (In prod
# this points off-box; here the whole point is that MinIO outlives the DB.)
export MC_HOST_backup="http://${OPN_S3_KEY}:${OPN_S3_SECRET}@minio:9000"
export OPN_BACKUP_MC_NETWORK="${PROJECT}_default"
export OPN_BACKUP_BUCKET=opn-backups
export OPN_BACKUP_RETAIN_DAYS=0   # no prune in the drill

. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
DRILL_DIR="$(cd "$(dirname "$0")" && pwd)"

FAILED=0
pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1" >&2; FAILED=1; }
assert_eq() { # expected actual label
  if [ "$1" = "$2" ]; then pass "$3 ($2)"; else fail "$3 (expected $1, got $2)"; fi
}

cleanup() {
  echo "== teardown =="
  compose down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

# psql as the owner superuser (local socket, trust) — one scalar. `tail -n1`
# keeps only the last statement's result: a multi-statement query (SET; SELECT)
# also prints the "SET" command tag on stdout, which would otherwise concatenate.
pg() { compose exec -T "$OPN_PG_SERVICE" psql -U "$OPN_PG_USER" -d "$OPN_PG_DB" -tAc "$1" | tail -n1 | tr -d '[:space:]'; }
# psql as opn_app over TCP with its real password — exercises the least-priv
# runtime role and the restored GRANTs, not the owner.
pg_app() { compose exec -T -e PGPASSWORD="$OPN_DB_APP_PASSWORD" "$OPN_PG_SERVICE" \
  psql -U opn_app -h 127.0.0.1 -d "$OPN_PG_DB" -tAc "$1" | tail -n1 | tr -d '[:space:]'; }

wait_pg() {
  for _ in $(seq 1 30); do
    compose exec -T "$OPN_PG_SERVICE" pg_isready -U "$OPN_PG_USER" -d "$OPN_PG_DB" >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "postgres did not become ready" >&2; return 1
}
wait_core() {
  for _ in $(seq 1 60); do
    compose exec -T core curl -fsS http://localhost:8080/healthz >/dev/null 2>&1 && return 0
    sleep 2
  done
  echo "core did not become healthy" >&2; compose logs core >&2 || true; return 1
}

echo "== 1. source stack up (postgres + redis + minio + core) =="
compose up -d --build core
wait_core
echo "core healthz: $(compose exec -T core curl -fsS http://localhost:8080/healthz)"

echo "== 2. seed a world + a message =="
compose exec -T core opn-core admin create-tenant --name drill --new-world drillworld >/dev/null
WORLD=$(pg "SELECT id FROM worlds LIMIT 1")
echo "world = $WORLD"
compose exec -T "$OPN_PG_SERVICE" psql -U "$OPN_PG_USER" -d "$OPN_PG_DB" -v ON_ERROR_STOP=1 >/dev/null <<SQL
INSERT INTO messages (id, world_id, channel_id, seq, sender_character, body, client_uuid)
VALUES (gen_random_uuid(), '$WORLD', gen_random_uuid(), 1, gen_random_uuid(),
        '{"text":"restore-drill smoke"}'::jsonb, gen_random_uuid());
SQL
SRC_MIG=$(pg "SELECT count(*) FROM _sqlx_migrations")
SRC_MSG=$(pg "SELECT count(*) FROM messages")
echo "source: _sqlx_migrations=$SRC_MIG messages=$SRC_MSG"

echo "== 3. backup (via pg-backup.sh, into MinIO) =="
mc_run mb --ignore-existing "backup/${OPN_BACKUP_BUCKET}" || true
BACKUP_OUT=$(bash "$DRILL_DIR/pg-backup.sh")
echo "$BACKUP_OUT"
KEY=$(printf '%s\n' "$BACKUP_OUT" | sed -n "s#^pg-backup: wrote backup/${OPN_BACKUP_BUCKET}/##p")
[ -n "$KEY" ] || { echo "could not parse backup key" >&2; exit 1; }
echo "backup key = $KEY"

echo "== 4. simulate DB loss (drop ONLY the pgdata volume; MinIO survives) =="
compose stop postgres core >/dev/null
compose rm -f postgres core >/dev/null
docker volume rm "${PROJECT}_pgdata" >/dev/null
echo "pgdata volume destroyed; backup still in MinIO:"
mc_run ls --recursive "backup/${OPN_BACKUP_BUCKET}/"

echo "== 5. fresh postgres (initdb re-seeds opn_app) =="
compose up -d postgres
wait_pg
FRESH_TABLES=$(pg "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'")
assert_eq "0" "$FRESH_TABLES" "fresh DB is empty before restore"

echo "== 6. restore (via pg-restore.sh) =="
bash "$DRILL_DIR/pg-restore.sh" "$KEY"

echo "== 7. verify =="
assert_eq "$SRC_MIG" "$(pg "SELECT count(*) FROM _sqlx_migrations")" "_sqlx_migrations round-trips"
assert_eq "$SRC_MSG" "$(pg "SELECT count(*) FROM messages")" "messages row count round-trips"
assert_eq "1" "$(pg_app "SET app.world_id='$WORLD'; SELECT count(*) FROM messages")" \
  "opn_app reads the message with its world set (RLS-served, restored)"
assert_eq "0" "$(pg_app "SET app.world_id='00000000-0000-0000-0000-000000000000'; SELECT count(*) FROM messages")" \
  "opn_app reads ZERO with a different world (RLS isolation intact post-restore)"

echo "== 8. real recovery smoke: Core boots against the restored DB =="
compose up -d core
if wait_core; then pass "Core Healthy on restored DB (no-op migrate, serves as opn_app)"; else fail "Core did not boot on restored DB"; fi

echo
if [ "$FAILED" -eq 0 ]; then echo "RESTORE DRILL: PASS"; else echo "RESTORE DRILL: FAIL" >&2; fi
exit "$FAILED"
