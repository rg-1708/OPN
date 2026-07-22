-- 0017_servers.sql — servers/guilds (contract gap #13, closes OPN-CORE.md §17 Q2).
-- A server is a membership umbrella over ordinary channels: server channels ARE
-- channels (send/history/receipts/reactions/resume/RLS all unchanged), and
-- channel_members rows for them are kept in sync with server_members by the
-- servers primitive. No new message plumbing.
--
-- Standard 0001 world-isolation convention (NULLIF form mandatory).

CREATE TABLE servers (
    id              uuid PRIMARY KEY,
    world_id        uuid NOT NULL REFERENCES worlds(id),
    name            text NOT NULL,
    -- No FK to media: the retention janitor deletes expired media rows and a
    -- dangling banner id must not block that. Validated live at write time;
    -- a later-expired banner just 404s on download.
    banner_media_id uuid,
    owner_character uuid NOT NULL REFERENCES characters(id),
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE server_members (
    server_id    uuid NOT NULL REFERENCES servers(id),
    world_id     uuid NOT NULL REFERENCES worlds(id),
    character_id uuid NOT NULL REFERENCES characters(id),
    joined_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (server_id, character_id)
);

-- servers.list looks up by member.
CREATE INDEX server_members_by_character ON server_members (world_id, character_id);

-- Container fields on channels: NULL server_id = a plain dm/group/sms thread,
-- exactly as before. category/position drive the client's channel tree only.
ALTER TABLE channels ADD COLUMN server_id uuid REFERENCES servers(id);
ALTER TABLE channels ADD COLUMN category  text;
ALTER TABLE channels ADD COLUMN position  integer NOT NULL DEFAULT 0;

-- Membership sync scans a server's channels on member add/remove.
CREATE INDEX channels_by_server ON channels (server_id) WHERE server_id IS NOT NULL;

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['servers', 'server_members'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY %I ON %I USING (world_id = NULLIF(current_setting(''app.world_id'', true), '''')::uuid)',
            t || '_world_isolation', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO opn_app', t);
    END LOOP;
END $$;
