#!/bin/sh
# Runs ONCE on a fresh Postgres data dir (docker-entrypoint-initdb.d), as the
# superuser, BEFORE Core's startup migration 0001 ever connects. It pre-creates
# the opn_app runtime role with the operator's OPN_DB_APP_PASSWORD so the
# least-privilege role has a REAL password from first boot.
#
# Migration 0001 (0001_roles_and_rls_groundwork.sql:41) guards role creation
# with `IF NOT EXISTS (... pg_roles ...)`, so it finds this role already present
# and skips its dev-only `PASSWORD 'opn'` seed — while still applying its GRANTs.
# That guard is exactly what makes this pre-seed safe.
#
# Role attributes MUST mirror 0001:42-48 (the source of truth): LOGIN,
# NOSUPERUSER, NOBYPASSRLS, NOCREATEDB, NOCREATEROLE. Keep the two in sync if the
# role's attributes ever change. `:'app_pw'` (psql-quoted) + `%L` escape the
# password safely, so a password containing quotes cannot break the statement.
set -e

if [ -z "$OPN_DB_APP_PASSWORD" ]; then
  echo "10-app-role-password.sh: OPN_DB_APP_PASSWORD is unset; refusing to seed opn_app with an empty password" >&2
  exit 1
fi

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
  --set app_pw="$OPN_DB_APP_PASSWORD" <<'SQL'
SELECT format(
  'CREATE ROLE opn_app LOGIN PASSWORD %L NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE',
  :'app_pw'
)
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'opn_app')
\gexec
SQL
