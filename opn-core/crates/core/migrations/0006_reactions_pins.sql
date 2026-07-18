-- 0006_reactions_pins.sql — Sprint 4 (OPN-CORE.md §10.2).
-- Deferred from Sprint 3 (reflections 2026-07-18, decision 5): these two
-- tables are unpartitioned and had no Sprint 3 consumer, so they land with
-- their handlers now. Standard 0001 world-isolation convention (NULLIF form).
--
-- Neither table foreign-keys `messages`: that table is range-partitioned, so
-- its PK is (id, created_at) and a plain (message_id) FK is impossible. The
-- handlers validate message existence with an RLS-scoped SELECT instead.

CREATE TABLE reactions (
    world_id     uuid NOT NULL REFERENCES worlds(id),
    channel_id   uuid NOT NULL REFERENCES channels(id),
    message_id   uuid NOT NULL,
    character_id uuid NOT NULL REFERENCES characters(id),
    emoji        text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    -- One row per (message, reactor, emoji): a repeat add is a no-op (§10.2).
    PRIMARY KEY (message_id, character_id, emoji)
);

-- Reaction reads/fan-out are per-message; the count query walks this.
CREATE INDEX reactions_by_message ON reactions (channel_id, message_id);

CREATE TABLE channel_pins (
    channel_id uuid NOT NULL REFERENCES channels(id),
    world_id   uuid NOT NULL REFERENCES worlds(id),
    message_id uuid NOT NULL,
    pinned_by  uuid NOT NULL REFERENCES characters(id),
    pinned_at  timestamptz NOT NULL DEFAULT now(),
    -- One pin per (channel, message); the 50-cap is enforced in-handler under
    -- the channel row lock (§10.2), not by a constraint.
    PRIMARY KEY (channel_id, message_id)
);

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['reactions', 'channel_pins'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY %I ON %I USING (world_id = NULLIF(current_setting(''app.world_id'', true), '''')::uuid)',
            t || '_world_isolation', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO opn_app', t);
    END LOOP;
END $$;
