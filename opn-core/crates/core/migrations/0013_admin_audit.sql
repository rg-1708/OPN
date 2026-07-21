-- 0013_admin_audit.sql — Sprint P0 (opn-panel-roadmap.md).
-- Operator action log for the admin panel. Not a world-scoped domain table
-- (no world_id), so it follows the worlds/tenants operator-table style from
-- 0003: no RLS, no opn_app grant. The admin router and CLI reach it only via
-- the owner role (OPN_MIGRATE_DATABASE_URL), same as `admin unfreeze`.
--
-- P0 only reads this (it stays empty); P1 writes one row per mutation. Raw API
-- keys never land here — `detail` carries only a fingerprint (cross-cutting
-- rule 2).

CREATE TABLE admin_audit (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at            timestamptz NOT NULL DEFAULT now(),
    action        text NOT NULL,
    target_tenant uuid REFERENCES tenants(id),
    detail        jsonb
);
-- Newest-first reads walk the PK (identity is monotonic) descending; no extra
-- index needed.
