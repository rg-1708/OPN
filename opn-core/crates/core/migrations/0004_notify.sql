-- 0004_notify.sql — Sprint 3 (OPN-CORE.md §10.8).
-- The inbox: durable landing spot for a notification whose recipient had no
-- live session at route time. Live recipients get a `notify.event` push
-- instead (notify::route decides); nothing is inboxed for them.
--
-- Standard 0001 world-isolation convention (NULLIF form is mandatory).

CREATE TABLE inbox (
    id           uuid PRIMARY KEY,
    world_id     uuid NOT NULL REFERENCES worlds(id),
    character_id uuid NOT NULL REFERENCES characters(id),
    -- Optional device target (§10.8 lists it); route inboxes at character
    -- scope for now, so this stays NULL until device-addressed notifications
    -- exist. Kept to match the design schema.
    device_id    uuid REFERENCES devices(id),
    app_id       text NOT NULL,
    kind         text NOT NULL,
    -- Semantic urgency (§10.8): ring | alert | silent. Presentation is the
    -- shell's job; Core stores the class only.
    class        text NOT NULL,
    payload      jsonb NOT NULL DEFAULT '{}'::jsonb,
    seen_at      timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- Read path: newest-first per recipient (GET /v1/notify/inbox). The cursor
-- idiom lands in Sprint 4; this index already supports keyset paging on it.
CREATE INDEX inbox_recipient ON inbox (world_id, character_id, created_at DESC, id DESC);

ALTER TABLE inbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE inbox FORCE ROW LEVEL SECURITY;
CREATE POLICY inbox_world_isolation ON inbox
    USING (world_id = NULLIF(current_setting('app.world_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON inbox TO opn_app;
