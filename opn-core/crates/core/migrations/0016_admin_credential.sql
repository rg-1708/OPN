-- 0016_admin_credential.sql — opn-panel-roadmap.md (supersedes env auth).
-- Admin panel password, set on FIRST LAUNCH via the panel's setup screen — not
-- env. The old ADMIN_PASSWORD_HASH shipped an argon2 PHC string
-- (`$argon2id$v=19$m=...`) through compose/.env interpolation, which shredded the
-- `$`-delimited fields so every login failed with "not a valid argon2 PHC
-- string". Storing the hash here removes env from the auth path entirely.
--
-- Operator table (like admin_audit, 0013): no world_id, no RLS, no opn_app
-- grant — the admin router reaches it only via the owner role
-- (OPN_MIGRATE_DATABASE_URL).
--
-- Singleton: the boolean PK defaulting true + CHECK (id) means at most one row
-- can ever exist, so setup is provably one-shot — the second INSERT conflicts on
-- the PK. First setter owns the panel; everyone after needs that password.

CREATE TABLE admin_credential (
    id            boolean PRIMARY KEY DEFAULT true CHECK (id),
    password_hash text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
