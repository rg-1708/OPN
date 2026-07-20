# Shared config + helpers for the Sprint 11 item-3 backup/restore scripts.
# Sourced, never executed. Every value is env-overridable so the identical
# scripts run against the prod stack, or a throwaway drill stack, unchanged.

# --- Compose targeting ------------------------------------------------------
# Default: the prod stack, resolved relative to this file (deploy/backup → ..).
# The drill and the operator set OPN_COMPOSE_PROJECT to name their stack.
OPN_CORE_DIR=${OPN_CORE_DIR:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"}
OPN_COMPOSE=${OPN_COMPOSE:-docker-compose.prod.yml}
OPN_COMPOSE_PROJECT=${OPN_COMPOSE_PROJECT:-}
OPN_PG_SERVICE=${OPN_PG_SERVICE:-postgres}
# opn_migrate is the cluster bootstrap superuser (the compose POSTGRES_USER), so
# it holds BYPASSRLS: it dumps every row and restores COPY past the domain
# tables' FORCE ROW LEVEL SECURITY. opn_app (NOBYPASSRLS) could do neither.
# Do NOT switch this to opn_app.
OPN_PG_USER=${OPN_PG_USER:-opn_migrate}
OPN_PG_DB=${OPN_PG_DB:-opn}

# --- Backup S3 target (mc alias `backup`) -----------------------------------
# Auth is supplied out of band as MC_HOST_backup=proto://KEY:SECRET@host (and,
# for the media mirror, MC_HOST_src for the primary MinIO). No `mc alias set`.
# The DB backup target MUST be a different failure domain than the primary
# MinIO (DB loss = fatal, roadmap §3) — an off-box S3/B2/second-host MinIO, not
# the same store the app already writes media to.
OPN_BACKUP_BUCKET=${OPN_BACKUP_BUCKET:-opn-backups}
OPN_BACKUP_PREFIX=${OPN_BACKUP_PREFIX:-postgres}
# Optional docker network so the throwaway mc container can reach an in-compose
# MinIO (the drill sets this to the stack network). Empty = default bridge,
# which is what an external HTTPS S3 endpoint needs.
OPN_BACKUP_MC_NETWORK=${OPN_BACKUP_MC_NETWORK:-}
MC_IMAGE=${MC_IMAGE:-minio/mc}

compose() {
  if [ -n "$OPN_COMPOSE_PROJECT" ]; then
    docker compose -p "$OPN_COMPOSE_PROJECT" -f "$OPN_CORE_DIR/$OPN_COMPOSE" "$@"
  else
    docker compose -f "$OPN_CORE_DIR/$OPN_COMPOSE" "$@"
  fi
}

# Run mc in a throwaway container with the `backup`/`src` aliases passed as
# MC_HOST_* env — no config volume, no persistent alias state. Creds are passed
# by NAME only (never `-e NAME=value`): docker forwards the value from its own
# environment, keeping the KEY:SECRET out of argv and /proc/<pid>/cmdline. Export
# so the name-only form actually forwards; `${VAR:+...}` omits an unset alias
# (pg-backup has no MC_HOST_src).
mc_run() {
  local netarg=()
  [ -n "$OPN_BACKUP_MC_NETWORK" ] && netarg=(--network "$OPN_BACKUP_MC_NETWORK")
  [ -n "${MC_HOST_backup:-}" ] && export MC_HOST_backup
  [ -n "${MC_HOST_src:-}" ] && export MC_HOST_src
  docker run --rm -i "${netarg[@]}" \
    ${MC_HOST_backup:+-e MC_HOST_backup} \
    ${MC_HOST_src:+-e MC_HOST_src} \
    "$MC_IMAGE" "$@"
}

require() {
  local v
  for v in "$@"; do
    if [ -z "${!v:-}" ]; then
      echo "$(basename "${0:-lib.sh}"): required env $v is unset" >&2
      exit 1
    fi
  done
}
