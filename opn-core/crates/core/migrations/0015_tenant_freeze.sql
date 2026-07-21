-- 0015_tenant_freeze.sql — Sprint P1 (opn-panel-roadmap.md).
-- Tenant-level freeze for the admin panel: set to now() to refuse NEW session
-- mints for the tenant; NULL = active. Distinct from `accounts.frozen_at`
-- (per-account balance freeze, `admin unfreeze`) — this gates the whole tenant's
-- key at the mint path.
--
-- Freeze/unfreeze are written by the admin router via the owner role. The app
-- role only reads it (mint_session gate), so opn_app gets SELECT on the new
-- column and nothing more.

ALTER TABLE tenants ADD COLUMN frozen_at timestamptz;

GRANT SELECT (frozen_at) ON tenants TO opn_app;
