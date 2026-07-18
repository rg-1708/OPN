-- 0001_roles_and_rls_groundwork.sql
-- OPN-CORE: multi-tenant, Postgres 16, row-level security by world_id.
-- Forward-only migration. No down migration.
--
-- ============================================================================
-- THE CONVENTION for every future domain-table migration.
-- Every domain table migration must include, in the same file:
--   ALTER TABLE t ENABLE ROW LEVEL SECURITY;
--   ALTER TABLE t FORCE ROW LEVEL SECURITY;
--   CREATE POLICY t_world_isolation ON t
--     USING (world_id = NULLIF(current_setting('app.world_id', true), '')::uuid);
--   GRANT SELECT, INSERT, UPDATE, DELETE ON t TO opn_app;
-- All runtime access goes through infra::db::world_tx() which runs
-- SET LOCAL app.world_id inside the transaction. FORCE RLS ensures even
-- the table owner is filtered. A query outside world_tx sees zero rows
-- (see NOTE below) rather than leaking.
--
-- NOTE: bare current_setting('app.world_id') ERRORS when the setting is unset,
-- which breaks even legitimate zero-row checks. Always use the two-arg form
-- current_setting('app.world_id', true) (missing_ok = true): it returns NULL
-- when unset, the comparison world_id = NULL is NULL → policy is false → the
-- query sees zero rows instead of erroring.
--
-- NOTE 2 (found by the canary test against a real Postgres): the two-arg form
-- is not enough on pooled connections. After ANY transaction has run
-- set_config(..., true) on a connection, the GUC reverts to an EMPTY STRING
-- at commit, not to "unset" — ''::uuid then errors 22P02 on every later
-- query on that connection. Hence the NULLIF(..., '') wrapper above; it is
-- mandatory, not defensive decoration.
-- ============================================================================

-- Application login role.
-- Roles are CLUSTER-WIDE, but this migration runs once PER DATABASE (the test
-- suite creates a fresh database per test and re-runs every migration). So the
-- CREATE ROLE must be idempotent — guard it with a pg_roles existence check.
--
-- Dev password only ('opn'). PRODUCTION must set a real password out-of-band:
--   ALTER ROLE opn_app PASSWORD '<secret>';
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'opn_app') THEN
        CREATE ROLE opn_app
            LOGIN
            PASSWORD 'opn'
            NOSUPERUSER
            NOBYPASSRLS
            NOCREATEDB
            NOCREATEROLE;
    END IF;
EXCEPTION WHEN duplicate_object OR unique_violation THEN
    -- Parallel per-test databases can race the existence check; the loser's
    -- CREATE ROLE failing is fine — the role exists. Two forms of the race:
    -- a serialized loser gets duplicate_object (42710); two truly-concurrent
    -- CREATE ROLEs that both pass the NOT EXISTS check collide at the
    -- cluster-wide pg_authid unique index instead, raising unique_violation
    -- (23505). Both mean "someone else created it" — swallow either.
    NULL;
END
$$;

-- GRANT CONNECT ON DATABASE cannot name the current database dynamically in
-- plain SQL, and every migration file is database-agnostic. Instead we grant
-- USAGE on the public schema and rely on the default PUBLIC connect privilege
-- that every role already has on every database.
GRANT USAGE ON SCHEMA public TO opn_app;
