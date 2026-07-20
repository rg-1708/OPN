#!/usr/bin/env bash
# Sprint 11 item 3 — media backup (best-effort; media loss is cosmetic, §3).
#
# Mirrors the primary MinIO media bucket to the backup target. Additive
# (`mc mirror` without --remove): never deletes backup objects whose source
# lifecycle-expired, so an expired-then-deleted original stays recoverable.
# Needs both aliases: MC_HOST_src (primary MinIO) and MC_HOST_backup.
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
require MC_HOST_src MC_HOST_backup

SRC_BUCKET=${OPN_MEDIA_BUCKET:-opn}
DST_BUCKET=${OPN_MEDIA_BACKUP_BUCKET:-opn-media-backup}

echo "media-mirror: src/${SRC_BUCKET} → backup/${DST_BUCKET}"
mc_run mb --ignore-existing "backup/${DST_BUCKET}"
mc_run mirror --overwrite "src/${SRC_BUCKET}" "backup/${DST_BUCKET}"
echo "media-mirror: done"
