#!/usr/bin/env bash
# Sprint 11 item 3 — scheduled Postgres backup (the non-negotiable one).
#
# Streams a compressed pg_dump of the opn DB straight into the backup S3 bucket
# via `mc pipe` — no temp file on disk, and no pg/mc tools needed on the host,
# only docker. Meant to run on a schedule (a Coolify Scheduled Task or cron; see
# docs/runbooks/restore-from-backup.md). Restore with pg-restore.sh; prove the
# whole pair end to end with restore-drill.sh.
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
require MC_HOST_backup
# A non-empty prefix is load-bearing: the retention prune below scopes to
# ${OPN_BACKUP_PREFIX}/, and an empty prefix would make it the whole bucket.
[ -n "$OPN_BACKUP_PREFIX" ] || { echo "pg-backup: OPN_BACKUP_PREFIX must be non-empty" >&2; exit 1; }

stamp=$(date -u +%Y%m%dT%H%M%SZ)
key="${OPN_BACKUP_PREFIX}/opn-${stamp}.dump"
echo "pg-backup: dumping ${OPN_PG_DB} → backup/${OPN_BACKUP_BUCKET}/${key}"

# Atomic publish: stream to <key>.partial, then rename to <key> only after a
# clean pipe (set -e + pipefail abort before the rename on any failure). So the
# bucket only ever holds COMPLETE .dump objects — a mid-dump crash leaves a
# .partial (swept by retention), never a truncated .dump a later DR might pick.
# -Fc = compressed custom format; the superuser owner dumps every row past RLS.
tmp="${key}.partial"
compose exec -T "$OPN_PG_SERVICE" pg_dump -U "$OPN_PG_USER" -Fc "$OPN_PG_DB" \
  | mc_run pipe "backup/${OPN_BACKUP_BUCKET}/${tmp}"
mc_run mv "backup/${OPN_BACKUP_BUCKET}/${tmp}" "backup/${OPN_BACKUP_BUCKET}/${key}"

echo "pg-backup: wrote backup/${OPN_BACKUP_BUCKET}/${key}"

# Retention prune. Default 30 days; 0 or a non-number disables it.
retain=${OPN_BACKUP_RETAIN_DAYS:-30}
case "$retain" in ''|*[!0-9]*)
  echo "pg-backup: OPN_BACKUP_RETAIN_DAYS='$retain' is not a number; skipping prune" >&2
  retain=0 ;;
esac
if [ "$retain" -gt 0 ]; then
  echo "pg-backup: pruning dumps older than ${retain}d"
  mc_run rm --recursive --force --older-than "${retain}d" \
    "backup/${OPN_BACKUP_BUCKET}/${OPN_BACKUP_PREFIX}/" \
    || echo "pg-backup: prune step failed (non-fatal)" >&2
fi
