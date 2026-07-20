#!/usr/bin/env bash
# Sprint 11 item 3 — restore a Postgres backup produced by pg-backup.sh.
#
# Streams the dump from the backup bucket straight into pg_restore. DESTRUCTIVE:
# it --cleans existing objects, so it refuses a non-empty target unless
# OPN_RESTORE_FORCE=1. The target's opn_app role MUST already exist — the dump
# carries GRANTs to opn_app, not the role itself. On a fresh prod stack the
# initdb pre-seed (deploy/postgres/initdb) creates opn_app before this runs.
#
# After a successful restore the _sqlx_migrations table is already populated, so
# starting Core is a no-op migrate — it serves the restored data as opn_app.
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
require MC_HOST_backup

key=${1:-}
if [ -z "$key" ]; then
  echo "usage: pg-restore.sh <object-key>" >&2
  echo "       e.g. pg-restore.sh postgres/opn-20260720T101500Z.dump" >&2
  echo "available dumps:" >&2
  mc_run ls --recursive "backup/${OPN_BACKUP_BUCKET}/${OPN_BACKUP_PREFIX}/" >&2 || true
  exit 2
fi

# Safety gate: refuse to clobber a populated DB unless explicitly forced.
existing=$(compose exec -T "$OPN_PG_SERVICE" \
  psql -U "$OPN_PG_USER" -d "$OPN_PG_DB" -tAc \
  "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'" \
  | tr -dc '0-9')
if [ "${existing:-0}" -gt 0 ] && [ "${OPN_RESTORE_FORCE:-0}" != "1" ]; then
  echo "pg-restore: target ${OPN_PG_DB} already has ${existing} public tables." >&2
  echo "            Restore is destructive (--clean). Set OPN_RESTORE_FORCE=1 to proceed." >&2
  exit 1
fi

echo "pg-restore: restoring backup/${OPN_BACKUP_BUCKET}/${key} → ${OPN_PG_DB}"
# --single-transaction: the whole DROP+CREATE+COPY runs as ONE transaction, so
#   any failure (a truncated dump, a missing role, lock contention with a live
#   Core) rolls the target back to exactly its pre-restore state — never a
#   half-dropped DB. It implies --exit-on-error and reads a -Fc archive fine from
#   the non-seekable stdin pipe (sequential; only -j parallelism can't). For an
#   IN-PLACE forced restore (OPN_RESTORE_FORCE=1), stop Core first so it is not
#   holding locks against the DROPs.
# --clean --if-exists: re-runnable (drop-then-recreate), no error on a fresh DB.
# --no-owner is NOT passed: restoring as the superuser owner keeps object
#   ownership on opn_migrate (as in prod) and applies the GRANTs to opn_app.
mc_run cat "backup/${OPN_BACKUP_BUCKET}/${key}" \
  | compose exec -T "$OPN_PG_SERVICE" \
    pg_restore -U "$OPN_PG_USER" -d "$OPN_PG_DB" --clean --if-exists --single-transaction

echo "pg-restore: done. Restart Core — migrations are already applied in the"
echo "            restored _sqlx_migrations, so startup migrate is a no-op."
