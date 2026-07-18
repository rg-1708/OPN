-- 0003_identity.sql — Sprint 1 (OPN-CORE.md §10.1).
-- worlds/tenants are NOT world-scoped domain rows: no RLS, no broad grants —
-- opn_app gets only the columns infra reads (api-key auth, world listing).
-- Everything else follows the 0001 convention (NULLIF form is mandatory).

CREATE TABLE worlds (
    id          uuid PRIMARY KEY,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE tenants (
    id              uuid PRIMARY KEY,
    name            text NOT NULL,
    -- sha256 hex of the raw `opn_...` key; high-entropy, so the hash lookup
    -- itself is the auth (OPN-CORE.md §11 — no KDF).
    api_key_hash    text NOT NULL UNIQUE,
    allowed_origins text[] NOT NULL DEFAULT '{}',
    world_id        uuid NOT NULL REFERENCES worlds(id),
    created_at      timestamptz NOT NULL DEFAULT now()
);

GRANT SELECT (id, name) ON worlds TO opn_app;
GRANT SELECT (id, name, api_key_hash, allowed_origins, world_id) ON tenants TO opn_app;

CREATE TABLE characters (
    id              uuid PRIMARY KEY,
    world_id        uuid NOT NULL REFERENCES worlds(id),
    framework_ref   text NOT NULL,
    number          text,
    last_seen_at    timestamptz,
    share_presence  boolean NOT NULL DEFAULT true,
    settings        jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (world_id, framework_ref)
);

-- Partial unique: assigned numbers are unique per world; NULL = not yet
-- assigned. The concurrent-mint retry loop leans on this index.
CREATE UNIQUE INDEX characters_world_number
    ON characters (world_id, number) WHERE number IS NOT NULL;

CREATE TABLE devices (
    id              uuid PRIMARY KEY,
    world_id        uuid NOT NULL REFERENCES worlds(id),
    owner_character uuid NOT NULL REFERENCES characters(id),
    kind            text NOT NULL,
    settings        jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE app_accounts (
    id           uuid PRIMARY KEY,
    world_id     uuid NOT NULL REFERENCES worlds(id),
    character_id uuid NOT NULL REFERENCES characters(id),
    app_id       text NOT NULL,
    handle       text NOT NULL,
    meta         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (world_id, app_id, handle)
);

CREATE TABLE sessions (
    id           uuid PRIMARY KEY,
    tenant_id    uuid NOT NULL REFERENCES tenants(id),
    world_id     uuid NOT NULL REFERENCES worlds(id),
    character_id uuid NOT NULL REFERENCES characters(id),
    device_id    uuid NOT NULL REFERENCES devices(id),
    -- Active app account per app, per session (OPN.md §3): { app_id: account_id }.
    app_accounts jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    revoked_at   timestamptz
);

-- Janitor sweep (expired-session delete) walks this.
CREATE INDEX sessions_expires_at ON sessions (expires_at);

-- 30-day cooldown before a freed number is reassignable (OPN-CORE.md §10.1).
CREATE TABLE retired_numbers (
    world_id uuid NOT NULL REFERENCES worlds(id),
    number   text NOT NULL,
    freed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (world_id, number)
);

-- Standard world-isolation RLS (0001 convention) for every domain table.
DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['characters', 'devices', 'app_accounts', 'sessions', 'retired_numbers'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY %I ON %I USING (world_id = NULLIF(current_setting(''app.world_id'', true), '''')::uuid)',
            t || '_world_isolation', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO opn_app', t);
    END LOOP;
END $$;
