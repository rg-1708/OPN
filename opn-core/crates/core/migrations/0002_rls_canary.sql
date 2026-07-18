-- 0002_rls_canary.sql
-- OPN-CORE: forward-only migration. No down migration.
--
-- _rls_canary is a PERMANENT tiny table that exists solely so tests can prove
-- the RLS pattern from 0001 works end-to-end: seed rows for two worlds, open a
-- world_tx, and assert only the current world's rows are visible/writable.
-- It applies the full convention documented in 0001.

CREATE TABLE _rls_canary (
    id       uuid PRIMARY KEY,
    world_id uuid NOT NULL,
    note     text NOT NULL
);

ALTER TABLE _rls_canary ENABLE ROW LEVEL SECURITY;
ALTER TABLE _rls_canary FORCE ROW LEVEL SECURITY;

-- Read AND write isolation: USING filters reads/updates/deletes, WITH CHECK
-- rejects inserts/updates that would place a row outside the current world.
CREATE POLICY _rls_canary_world_isolation ON _rls_canary
    FOR ALL
    USING (world_id = NULLIF(current_setting('app.world_id', true), '')::uuid)
    WITH CHECK (world_id = NULLIF(current_setting('app.world_id', true), '')::uuid);

GRANT SELECT, INSERT, UPDATE, DELETE ON _rls_canary TO opn_app;
