# Runbook: restore from backup

Backs up and restores opn-core's Postgres (and, best-effort, media) via the
scripts in [opn-core/deploy/backup/](../../opn-core/deploy/backup). The DB backup
is the **non-negotiable**: DB loss is fatal, media loss is cosmetic.
One dump of the `opn` database, streamed
off-box, is the whole recovery story — everything else here exists to make that
dump restorable and to prove it.

## 1. What gets backed up & where

| What | Script | How | Target |
|---|---|---|---|
| Postgres (the whole `opn` DB) | [pg-backup.sh](../../opn-core/deploy/backup/pg-backup.sh) | custom-format `pg_dump -Fc` streamed via `mc pipe` (no temp file, no host pg/mc tools) | `backup/${OPN_BACKUP_BUCKET}/${OPN_BACKUP_PREFIX}/opn-<UTC-stamp>.dump`, then prunes dumps older than `OPN_BACKUP_RETAIN_DAYS` (default 30d) |
| Media objects | [media-mirror.sh](../../opn-core/deploy/backup/media-mirror.sh) | `mc mirror --overwrite` (additive — no `--remove`, so expired-then-deleted originals stay recoverable) | `backup/${OPN_MEDIA_BACKUP_BUCKET}` |

> **CRITICAL durability rule** ([lib.sh](../../opn-core/deploy/backup/lib.sh)
> comment): the DB backup target (`backup` alias) **MUST** be a *different failure
> domain* than the primary MinIO the app writes media to — an off-box S3 / B2 /
> second-host MinIO, never the same store. A backup in the same failure domain as
> the DB it protects is not a backup.

## 2. Required environment

All defaults live in [lib.sh](../../opn-core/deploy/backup/lib.sh); each script
reads the same set. Auth is supplied out of band as `MC_HOST_*` env (no
`mc alias set`). The scripts need **no pg or mc tools on the host** — `pg_dump` /
`pg_restore` / `psql` run via `docker compose exec` into the `postgres` service,
and every `mc` call runs in a throwaway `minio/mc` container.

| Var | Default | Consumed by |
|---|---|---|
| `MC_HOST_backup` | — (required) | the `backup` mc alias, form `proto://KEY:SECRET@host`. Every script (`require`d by all three). |
| `MC_HOST_src` | — (required, media only) | the `src` alias = primary MinIO. media-mirror.sh only. |
| `OPN_COMPOSE` | `docker-compose.prod.yml` | compose file name passed to `docker compose -f`. |
| `OPN_COMPOSE_PROJECT` | *(empty)* | `docker compose -p` project name; empty = default project. |
| `OPN_CORE_DIR` | resolved to the `opn-core` dir (`deploy/backup/../..`) | where `compose()` finds `$OPN_COMPOSE`. |
| `OPN_PG_SERVICE` | `postgres` | compose service to `exec` pg tools into. |
| `OPN_PG_USER` | `opn_migrate` | `-U` for `pg_dump` / `pg_restore` / `psql`. The owner superuser (BYPASSRLS) — do NOT set to `opn_app`. |
| `OPN_PG_DB` | `opn` | target database. |
| `OPN_BACKUP_BUCKET` | `opn-backups` | dump bucket on the `backup` alias. |
| `OPN_BACKUP_PREFIX` | `postgres` | key prefix under the bucket. |
| `OPN_BACKUP_MC_NETWORK` | *(empty)* | docker network for the throwaway mc container; empty = default bridge (what an external HTTPS S3 endpoint needs). Set only to reach an in-compose MinIO. |
| `MC_IMAGE` | `minio/mc` | image for the throwaway mc container. |
| `OPN_BACKUP_RETAIN_DAYS` | `30` | prune horizon in pg-backup.sh; `0` disables the prune. |
| `OPN_MEDIA_BUCKET` | `opn` | media source bucket (media-mirror.sh). |
| `OPN_MEDIA_BACKUP_BUCKET` | `opn-media-backup` | media destination bucket (media-mirror.sh). |
| `OPN_RESTORE_FORCE` | `0` | set `1` to override pg-restore.sh's non-empty-target safety gate (§5). |

## 3. Scheduling the backup

[pg-backup.sh](../../opn-core/deploy/backup/pg-backup.sh) is meant to run on a
schedule; [media-mirror.sh](../../opn-core/deploy/backup/media-mirror.sh) can ride
the same schedule. The exact scheduler is operator/host-specific — a Coolify
Scheduled Task or a host cron entry both work. Whatever you use, it must run from
the `opn-core` dir with `MC_HOST_backup` (and, for media, `MC_HOST_src`) in the
environment, and on the dev host go through the docker group with `sg docker -c`.

```bash
# host cron (crontab -e), nightly 03:15 UTC. Cron does NOT inherit your shell
# env, so source the backup creds from a root-only env file first. Adjust the
# path, schedule, and scheduler to the host.
15 3 * * *  cd /srv/opn-core && set -a && . /etc/opn/backup.env && set +a && \
            sg docker -c 'bash deploy/backup/pg-backup.sh' >> /var/log/opn-pg-backup.log 2>&1

# optional, best-effort media, needs MC_HOST_src too:
30 3 * * *  cd /srv/opn-core && set -a && . /etc/opn/backup.env && set +a && \
            sg docker -c 'bash deploy/backup/media-mirror.sh' >> /var/log/opn-media-mirror.log 2>&1
```

`set -o pipefail` makes a mid-dump failure exit non-zero, so a failed run surfaces
to the scheduler — wire the scheduler's failure alert. The prune step failing is
logged but non-fatal.

## 4. Restoring

Recovery uses [pg-restore.sh](../../opn-core/deploy/backup/pg-restore.sh), which
streams a dump from the backup bucket straight into `pg_restore`.

**Preconditions that matter:**

- **`opn_app` must already exist on the target.** The dump carries the GRANTs to
  `opn_app`, not the role itself. On a fresh prod stack the initdb pre-seed
  ([10-app-role-password.sh](../../opn-core/deploy/postgres/initdb/10-app-role-password.sh))
  creates `opn_app` with `OPN_DB_APP_PASSWORD` before restore runs. Bring up a
  fresh Postgres before restoring so this runs.
- **Restore runs as `opn_migrate`** (the container superuser / `POSTGRES_USER`,
  owner, BYPASSRLS). That is what lets `pg_restore`'s COPY load rows *past* the
  domain tables' `FORCE ROW LEVEL SECURITY`
  ([0001_roles_and_rls_groundwork.sql](../../opn-core/crates/core/migrations/0001_roles_and_rls_groundwork.sql)) —
  the least-privilege `opn_app` (NOSUPERUSER, NOBYPASSRLS) could not. `--no-owner`
  is intentionally NOT passed, so object ownership stays on `opn_migrate` as in
  prod and the GRANTs land on `opn_app`.
- **The safety gate** refuses a non-empty target unless `OPN_RESTORE_FORCE=1`,
  because restore is `--clean` (drop-then-recreate). Fresh volume = empty = no
  flag needed.

**Steps:**

```bash
cd /srv/opn-core                 # OPN_CORE_DIR; export MC_HOST_backup first

# 1. Bring up a fresh Postgres (empty pgdata volume → initdb creates opn_app).
sg docker -c 'docker compose -f docker-compose.prod.yml up -d postgres'

# 2. List available dumps (running pg-restore.sh with no key prints them, exit 2).
sg docker -c 'bash deploy/backup/pg-restore.sh'

# 3. Restore a chosen dump by its object key.
sg docker -c 'bash deploy/backup/pg-restore.sh postgres/opn-20260720T101500Z.dump'

# 4. Start Core. Its migrations are already in the restored _sqlx_migrations, so
#    the startup migrate is a no-op; Core then serves the restored data as opn_app.
sg docker -c 'docker compose -f docker-compose.prod.yml up -d core'
```

Verify Core came up healthy per [incident-triage.md](incident-triage.md) §1
(`/healthz`). Note the prod stack publishes **no host ports** — only Traefik
routes to `core:8080`
([docker-compose.prod.yml](../../opn-core/docker-compose.prod.yml)) — so hit
`/healthz` from inside the network (`docker compose exec core curl -fsS
http://localhost:8080/healthz`).

> `pg_restore --clean` is **destructive** — it drops existing objects before
> recreating them; that is why the gate exists. The restore runs as one
> `--single-transaction`, so a failure rolls the target back to its pre-restore
> state (never a half-dropped DB) — but for an **in-place forced restore**
> (`OPN_RESTORE_FORCE=1` over a live stack) **stop Core first**, or its open
> connections hold locks against the DROPs. And the `initdb` pre-seed of
> `opn_app` runs **only on an empty `pgdata` volume** (same caveat as
> [deploy-rollback.md](deploy-rollback.md) §2). Restoring into an already-populated
> volume skips the pre-seed *and* trips the safety gate — restore into a fresh
> volume. If you must restore over an existing volume where `opn_app` is absent,
> create the role by hand first (`ALTER ROLE`/`CREATE ROLE` as the owner) or the
> GRANTs in the dump have no target.

## 5. Verifying — the drill

*An untested backup is a wish, not a backup.*
[restore-drill.sh](../../opn-core/deploy/backup/restore-drill.sh) is the periodic
proof: it scripts one full **backup → destroy the DB volume → restore into a fresh
stack → verify** cycle, reusing the real `pg-backup.sh` and `pg-restore.sh`, and
asserts the outcome by exit code. It models the actual failure domain — only the
Postgres volume is destroyed; the backup lives in MinIO, which survives.

Per its header comment, the drill asserts:

- `_sqlx_migrations` row count survives the round-trip (⇒ the real recovery path —
  a no-op startup migrate — works).
- the seeded message row survives (row-count smoke).
- `opn_app` (least-priv, real password, over TCP) reads the message back with its
  world set, and reads **zero** with a different world — RLS is intact post-restore.
- Core boots **Healthy** against the restored DB and serves as `opn_app`.

Run it (Docker-only; on the dev host go through the docker group):

```bash
sg docker -c 'bash opn-core/deploy/backup/restore-drill.sh'
```

It runs entirely in a throwaway isolated compose project (`opn-restore-drill`) on
its own volumes and tears itself down on exit, so it never touches the dev or prod
stack. Exit `0` = `RESTORE DRILL: PASS`.
