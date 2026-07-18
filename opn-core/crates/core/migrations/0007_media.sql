-- 0007_media.sql — Sprint 5 (OPN-CORE.md §10.6). Presigned-upload media with a
-- pending→live lifecycle and a janitor verification sweep. Bytes flow
-- client↔MinIO directly; Core only records rows and issues policies. Standard
-- 0001 world-isolation convention (NULLIF form).

CREATE TABLE media (
    id              uuid NOT NULL PRIMARY KEY,
    world_id        uuid NOT NULL REFERENCES worlds(id),
    owner_character uuid NOT NULL REFERENCES characters(id),
    kind            text NOT NULL,           -- photo|video|audio
    mime            text NOT NULL,
    -- Client-declared size. The POST policy caps the upload to this at MinIO;
    -- the verify sweep re-checks the real object against it (cap bypass → revert).
    bytes           bigint NOT NULL,
    -- pending: policy issued, object may or may not exist yet.
    -- live:    committed by the owner; attachable to messages, shown in gallery.
    state           text NOT NULL DEFAULT 'pending',
    has_thumb       boolean NOT NULL DEFAULT false,
    -- Last successful verify HEAD. NULL = never verified → swept first (so a
    -- freshly committed object is confirmed to actually exist within a tick).
    verified_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- Own-gallery reads (media.list): newest-first over the owner; the cursor idiom
-- keysets on (created_at, id) here.
CREATE INDEX media_owner_live ON media (owner_character, created_at DESC, id DESC)
    WHERE state = 'live';
-- Verify sweep cursor: live rows by (verified_at NULLS FIRST, id) within a
-- world so the sweep is incremental and never a full table scan per tick.
CREATE INDEX media_verify_cursor ON media (world_id, verified_at NULLS FIRST, id)
    WHERE state = 'live';
-- Pending reap: pending rows by age.
CREATE INDEX media_pending_age ON media (created_at) WHERE state = 'pending';

ALTER TABLE media ENABLE ROW LEVEL SECURITY;
ALTER TABLE media FORCE ROW LEVEL SECURITY;
CREATE POLICY media_world_isolation ON media
    USING (world_id = NULLIF(current_setting('app.world_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON media TO opn_app;
